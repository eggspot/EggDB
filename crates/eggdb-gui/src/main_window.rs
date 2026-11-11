use std::time::Instant;

use eggdb_platform::window::{
    AccessAction, AccessActions, AccessCaret, AccessCell, AccessNode, AccessRect, AccessRole,
    AccessTable, AccessTreePosition, Key, KeyPress,
};

use crate::canvas::{Canvas, Rect};
use crate::catalog::{
    Catalog, CatalogColumn, CatalogIndex, CatalogTable, CatalogView, KeyCount, KeySpace,
};
use crate::color::{Rgba, Theme};
use crate::connection::SessionId;
use crate::dialect::QueryDialect;
use crate::drop_prompt::{self, DropAnswer, DropKind, DropPrompt, DropTarget};
use crate::editor::Position;
use crate::editor_menu::{self, EditorMenuOrigin};
use crate::font::{TextStyle, Typeface};
use crate::grid::{
    self, Cell, CellLook, ResultGrid, ValueKind, cell_text, quote_name, visible_columns,
    visible_rows,
};
use crate::grid_menu::{self, GridMenu};
use crate::highlight::{Carry, TokenKind, tokens};
use crate::inspector::{self, Inspector, InspectorBlock};
use crate::key_scan::{KeySpaceScan, PagingButton};
use crate::layout::{Layout, LayoutAction};
use crate::layout_picker::{self, LayoutPicker, PickerOutcome};
use crate::object_browser::{
    self, Group, ObjectTree, ROW_HEIGHT, ROWS_IN_VIEW, ROWS_TOP, RowIcon, SidebarFocus, TreeAction,
    TreeItem,
};
use crate::plan_view::{self, PlanView};
use crate::pointer::{self, CaretLook, Fill, FillMix, PointerLook};
use crate::query_tab::{QueryTab, ResultView, TabAction, TabFocus};
use crate::scrollbar::{self, Axis, Extent};
use crate::splitters::{self, RAIL_WIDTH, RegionSizes, Splitter};
use crate::status_line;
use crate::tab_strip::{self, ClosePrompt, PromptAnswer};
use crate::tree_menu::{self, TreeMenu, TreeMenuAnswer, TreeMenuItem, TreeMenuOrigin};
use crate::value_view;

/// The width of the results grid's body, right of the row gutter, at the golden frame's size.
pub const GRID_BODY_WIDTH: f32 = 972.0;
const GRID_LEFT: f32 = 308.0;
const BODY_BOTTOM: f32 = 776.0;
const EDITOR_TEXT_TOP: f32 = 88.0;
const EDITOR_TEXT_LEFT: f32 = 320.0;
const EDITOR_GUTTER_RIGHT: f32 = 308.0;
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
#[allow(clippy::large_enum_variant)]
pub enum WindowAction {
    /// The query tab's answer.
    Tab(TabAction),
    /// Fetch the sample of the row the inspector shows.
    Inspect(TreeItem),
    /// Read the catalog again.
    RefreshCatalog,
    /// Run a drop from the tree in a session of its own (`docs/ux/workspace.md` 10.2).
    Drop {
        /// The statement.
        statement: String,
        /// The dialect it is written in.
        dialect: QueryDialect,
    },
    /// The layout picker applied this layout: the client keeps it in its settings.
    LayoutChosen(Layout),
    /// A tab closed: end its kept session (rolling back its transaction), after stopping its
    /// run when `stop`.
    CloseSession {
        /// The closed tab's session.
        session: SessionId,
        /// Whether its statement was running and must be stopped first.
        stop: bool,
    },
}

/// The status bar's notice when a run is asked for while another tab's statement runs.
pub fn another_tab_running(title: &str) -> String {
    format!("{title} is running: wait for it, or stop it with Esc in its tab")
}

/// The status bar's notice while a run keeps the tree from opening another tab.
pub const OPEN_WHILE_RUNNING: &str =
    "A query is running: press Esc to stop it, then open the table again";

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
    /// The other query tabs, in the strip's order; the one that shows sits at
    /// [`MainWindow::tab_position`] among them.
    pub background_tabs: Vec<QueryTab>,
    /// The showing tab's place in the strip.
    pub tab_position: usize,
    /// How far the tab strip is scrolled while its tabs overflow it (`docs/ux/workspace.md`
    /// 9.1).
    pub tab_scroll: f32,
    /// The session the next new tab gets.
    pub next_session: SessionId,
    /// The dialog asking before the showing tab closes, while it is open.
    pub close_prompt: Option<ClosePrompt>,
    /// The pointer's hover, press and whether it moved the focus last.
    pub pointer: PointerLook,
    /// How the editor's caret draws at this moment of its blink.
    pub caret: CaretLook,
    /// The rows and columns the wheel moved by that did not add up to a whole step yet, down
    /// and right, so a touchpad's small deltas still scroll.
    pub wheel_carry: (f32, f32),
    /// The layout of the regions, labels and keys (`docs/ux/gui-layouts.md`).
    pub layout: Layout,
    /// The layout picker, while it is open over the window.
    pub layout_picker: Option<LayoutPicker>,
    /// Whether `Ctrl/Cmd+K` was pressed and the next key finishes the chord (`L` opens the
    /// layout picker).
    pub chord_started: bool,
    /// The grid column whose right edge the pointer is over or drags (`docs/ux/workspace.md`
    /// 9.5).
    pub column_edge: Option<usize>,
    /// Whether that edge is being dragged.
    pub resizing: bool,
    /// The grid scrollbar the pointer is over or drags, drawn wider (9.6).
    pub scrollbar_hot: Option<Axis>,
    /// The object tree's open row menu (`docs/ux/workspace.md` 10.1).
    pub tree_menu: Option<TreeMenu>,
    /// The dialog confirming a drop, while it is open (10.2), with the row selected after
    /// the drop succeeds.
    pub drop_prompt: Option<(DropPrompt, TreeItem)>,
    /// The sizes the splitters and `Ctrl/Cmd+B` set (`docs/ux/workspace.md` 10.5, 10.6).
    pub sizes: RegionSizes,
    /// Whether the sizes changed since the client last kept them in its settings.
    pub sizes_changed: bool,
    /// The splitter with the keyboard focus.
    pub focused_splitter: Option<Splitter>,
    /// The splitter the pointer is over or drags.
    pub hot_splitter: Option<Splitter>,
    /// The splitter being dragged.
    pub dragged_splitter: Option<Splitter>,
    /// Whether the sidebar's drawer is open over the rail and the centre (10.6).
    pub drawer_open: bool,
    /// The rail item the drawer was opened from.
    pub drawer_from: Option<RailItem>,
    /// Whether the clipboard held text when the last key or right-click came, which
    /// enables the editor menu's `Paste` on the desktop (`docs/ux/workspace.md` 10.9).
    pub clipboard_has_text: bool,
}

/// One of the rail's buttons while the sidebar is collapsed (`docs/ux/workspace.md` 10.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailItem {
    /// Opens the drawer with the tree focused.
    Objects,
    /// Opens the drawer with the filter field focused.
    Filter,
    /// Opens the drawer with this group selected and expanded.
    Group(Group),
}

impl RailItem {
    /// The item's name, as its tooltip and a screen reader give it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Objects => "Objects",
            Self::Filter => "Filter objects",
            Self::Group(Group::Keys) => "Keys",
            Self::Group(_) => "Tables",
        }
    }
}

const RAIL_FIRST_TOP: f32 = 52.0;
const RAIL_BUTTON_HEIGHT: f32 = 32.0;
const RAIL_SEPARATOR_SPACE: f32 = 9.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Area {
    Toolbar,
    Sidebar,
    Tabs,
    Editor,
    Results,
    StatusBar,
}

const AREAS: [Area; 6] = [
    Area::Toolbar,
    Area::Sidebar,
    Area::Tabs,
    Area::Editor,
    Area::Results,
    Area::StatusBar,
];
const SIDEBAR_RIGHT: f32 = 260.0;
const EDITOR_TOP: f32 = 80.0;
const EDITOR_HEIGHT: f32 = 300.0;
const SPLITTER_TOP: f32 = 380.0;
const TOOLBAR_HEIGHT: f32 = 44.0;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Placed {
    shift: (f32, f32),
    clip: Rect,
}

fn area_at(x: f32, y: f32) -> Area {
    if y < TOOLBAR_HEIGHT {
        Area::Toolbar
    } else if y >= BODY_BOTTOM {
        Area::StatusBar
    } else if x < SIDEBAR_RIGHT {
        Area::Sidebar
    } else if y < EDITOR_TOP {
        Area::Tabs
    } else if y < SPLITTER_TOP {
        Area::Editor
    } else {
        Area::Results
    }
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
            tab: QueryTab {
                session: SessionId(1),
                ..QueryTab::new("Query 1")
            },
            background_tabs: Vec::new(),
            tab_position: 0,
            tab_scroll: 0.0,
            next_session: SessionId(2),
            close_prompt: None,
            pointer: PointerLook::default(),
            caret: CaretLook::On,
            wheel_carry: (0.0, 0.0),
            layout: Layout::EggDb,
            layout_picker: None,
            chord_started: false,
            column_edge: None,
            resizing: false,
            scrollbar_hot: None,
            tree_menu: None,
            drop_prompt: None,
            sizes: RegionSizes::default(),
            sizes_changed: false,
            focused_splitter: None,
            hot_splitter: None,
            dragged_splitter: None,
            drawer_open: false,
            drawer_from: None,
            clipboard_has_text: true,
        }
    }

    /// Whether the sidebar shows as the 36 px rail: collapsed by `Ctrl/Cmd+B`, or always in
    /// a viewport narrower than 800 px.
    pub fn rail_shown(&self) -> bool {
        self.layout == Layout::EggDb
            && (!self.sizes.sidebar_shown || self.width < crate::connect_screen::MIN_WIDTH as usize)
    }

    /// Where the centre starts: the sidebar's or the rail's right edge.
    pub fn sidebar_right(&self) -> f32 {
        if self.layout != Layout::EggDb {
            SIDEBAR_RIGHT
        } else if self.rail_shown() {
            RAIL_WIDTH
        } else {
            self.sizes.sidebar_width
        }
    }

    fn centre_shift(&self) -> f32 {
        self.sidebar_right() - SIDEBAR_RIGHT
    }

    fn results_lift(&self) -> f32 {
        if self.layout == Layout::EggDb {
            self.sizes.results_band_top() - SPLITTER_TOP
        } else {
            0.0
        }
    }

    fn sizes_moved(&self) -> bool {
        self.layout == Layout::EggDb && (self.centre_shift() != 0.0 || self.results_lift() != 0.0)
    }

    fn docked_width(&self) -> f32 {
        if self.inspector.docked {
            self.sizes.inspector_width
        } else {
            0.0
        }
    }

    fn sized_placement(&self, area: Area) -> Placed {
        let left = self.sidebar_right();
        let dx = left - SIDEBAR_RIGHT;
        let right = self.centre_right() + dx;
        let band = self.sizes.results_band_top();
        let centre = (right - left).max(0.0);
        let full = Rect::new(0.0, 0.0, self.width as f32, HEIGHT as f32);
        let (shift, clip) = match area {
            Area::Toolbar | Area::StatusBar => ((0.0, 0.0), full),
            Area::Sidebar => (
                (0.0, 0.0),
                Rect::new(0.0, TOOLBAR_HEIGHT, left, BODY_BOTTOM - TOOLBAR_HEIGHT),
            ),
            Area::Tabs => (
                (dx, 0.0),
                Rect::new(left, TOOLBAR_HEIGHT, centre, EDITOR_TOP - TOOLBAR_HEIGHT),
            ),
            Area::Editor => (
                (dx, 0.0),
                Rect::new(left, EDITOR_TOP, centre, band - EDITOR_TOP),
            ),
            Area::Results => (
                (dx, band - SPLITTER_TOP),
                Rect::new(left, band, centre, BODY_BOTTOM - band),
            ),
        };
        Placed { shift, clip }
    }

    /// The band `splitter` is grabbed by, in window coordinates; `None` while it does not
    /// show (the sidebar's under the rail, the inspector's unless it is docked).
    pub fn splitter_band(&self, splitter: Splitter) -> Option<Rect> {
        if self.layout != Layout::EggDb {
            return None;
        }
        let width = self.width as f32;
        let body = BODY_BOTTOM - TOOLBAR_HEIGHT;
        match splitter {
            Splitter::Sidebar if !self.rail_shown() => Some(Rect::new(
                self.sizes.sidebar_width - 4.0,
                TOOLBAR_HEIGHT,
                splitters::BAND,
                body,
            )),
            Splitter::Results => {
                let left = self.sidebar_right();
                let right = self.centre_right() + self.centre_shift();
                Some(Rect::new(
                    left,
                    self.sizes.results_band_top(),
                    (right - left).max(0.0),
                    splitters::BAND,
                ))
            }
            Splitter::Inspector if self.inspector.docked => Some(Rect::new(
                width - self.sizes.inspector_width - 3.0,
                TOOLBAR_HEIGHT,
                splitters::BAND,
                body,
            )),
            Splitter::Sidebar | Splitter::Inspector => None,
        }
    }

    /// The splitter whose band holds (`x`, `y`).
    pub fn splitter_at(&self, x: f32, y: f32) -> Option<Splitter> {
        [Splitter::Inspector, Splitter::Sidebar, Splitter::Results]
            .into_iter()
            .find(|&splitter| {
                self.splitter_band(splitter)
                    .is_some_and(|band| rect_contains(band, x, y))
            })
    }

    /// The rail's buttons, top to bottom: `Objects`, `Filter objects`, then the groups the
    /// connection has.
    pub fn rail_items(&self) -> Vec<RailItem> {
        let mut items = vec![RailItem::Objects, RailItem::Filter];
        if !self.tree.catalog.tables.is_empty() {
            items.push(RailItem::Group(Group::Tables));
        }
        if !self.tree.catalog.key_spaces.is_empty() {
            items.push(RailItem::Group(Group::Keys));
        }
        items
    }

    /// Where the rail's item at `index` starts.
    pub fn rail_item_top(index: usize) -> f32 {
        let separator = if index >= 2 {
            RAIL_SEPARATOR_SPACE
        } else {
            0.0
        };
        RAIL_FIRST_TOP + RAIL_BUTTON_HEIGHT * index as f32 + separator
    }

    fn rail_item_at(&self, y: f32) -> Option<RailItem> {
        self.rail_items()
            .into_iter()
            .enumerate()
            .find(|&(index, _)| {
                let top = Self::rail_item_top(index);
                y >= top && y < top + RAIL_BUTTON_HEIGHT
            })
            .map(|(_, item)| item)
    }

    /// Opens the drawer from the rail's `item` (`docs/ux/workspace.md` 10.6).
    pub fn open_drawer(&mut self, item: RailItem) {
        self.drawer_open = true;
        self.drawer_from = Some(item);
        match item {
            RailItem::Objects => self.focus_tree(),
            RailItem::Filter => {
                self.focus_tree();
                self.focus = WindowFocus::Filter;
            }
            RailItem::Group(group) => {
                self.focus_tree();
                let row = TreeItem::Group(group);
                self.tree.expanded.insert(row);
                self.tree.current = row;
                self.tree.selected = Some(row);
            }
        }
    }

    /// Closes the drawer, the focus going back to the editor.
    pub fn close_drawer(&mut self) {
        if std::mem::take(&mut self.drawer_open) {
            self.drawer_from = None;
            self.focus_tab();
        }
    }

    /// `Ctrl/Cmd+B`: collapses the sidebar to the rail and back; under 800 px, where the rail
    /// always shows, opens and closes the drawer.
    pub fn toggle_sidebar(&mut self) {
        if self.width < crate::connect_screen::MIN_WIDTH as usize {
            if self.drawer_open {
                self.close_drawer();
            } else {
                self.open_drawer(RailItem::Objects);
            }
            return;
        }
        self.drawer_open = false;
        self.drawer_from = None;
        self.sizes.sidebar_shown = !self.sizes.sidebar_shown;
        self.sizes_changed = true;
        if !self.sizes.sidebar_shown && self.focus != WindowFocus::Tab {
            self.focus_tab();
        }
        if self.focused_splitter == Some(Splitter::Sidebar) {
            self.focused_splitter = None;
        }
    }

    fn splitter_key(&mut self, splitter: Splitter, press: KeyPress) -> WindowAction {
        let none = WindowAction::Tab(TabAction::None);
        let back = press.key == Key::Escape || (press.key == Key::Tab && press.modifiers.shift);
        if back && splitter == Splitter::Sidebar {
            self.focus_tree();
            return none;
        }
        if back || press.key == Key::Tab {
            self.focus_tab();
            return none;
        }
        if let Some(changed) = self.sizes.key(splitter, press) {
            self.tab.notice = Some(self.sizes.notice(splitter));
            self.sizes_changed |= changed;
        }
        none
    }

    fn placed(&self, area: Area) -> Option<Placed> {
        let width = self.width as f32;
        if self.sizes_moved() {
            return Some(self.sized_placement(area));
        }
        if self.layout == Layout::EggDb {
            return Some(Placed {
                shift: (0.0, 0.0),
                clip: Rect::new(0.0, 0.0, width, HEIGHT as f32),
            });
        }
        let regions = self.layout.regions_at(width);
        let (target, from) = match area {
            Area::Toolbar => (regions.toolbar?, (0.0, 0.0)),
            Area::Sidebar => (regions.sidebar?, (0.0, TOOLBAR_HEIGHT)),
            Area::Tabs => (regions.tabs?, (SIDEBAR_RIGHT, TOOLBAR_HEIGHT)),
            Area::Editor => {
                let editor = regions.editor;
                let shown = Rect::new(editor.x, editor.y, editor.w, editor.h.min(EDITOR_HEIGHT));
                (shown, (SIDEBAR_RIGHT, EDITOR_TOP))
            }
            Area::Results => (regions.results, (SIDEBAR_RIGHT, SPLITTER_TOP)),
            Area::StatusBar => (regions.statusbar?, (0.0, BODY_BOTTOM)),
        };
        Some(Placed {
            shift: (target.x - from.0, target.y - from.1),
            clip: target,
        })
    }

    fn area_width(&self, area: Area) -> f32 {
        let width = self.width as f32;
        match self.placed(area) {
            Some(placed) if self.layout != Layout::EggDb => placed.clip.w,
            _ => width,
        }
    }

    fn place_nodes(&self, nodes: &mut [AccessNode]) {
        if self.layout == Layout::EggDb && !self.sizes_moved() {
            return;
        }
        let widened = if self.layout == Layout::EggDb {
            self.centre_shift().max(0.0)
        } else {
            0.0
        };
        let mut inspector = vec![NODE_INSPECTOR];
        for node in nodes {
            if inspector.contains(&node.id)
                || node
                    .parent
                    .is_some_and(|parent| inspector.contains(&parent))
            {
                inspector.push(node.id);
                continue;
            }
            let area = area_at(node.bounds.x, node.bounds.y);
            if area == Area::Sidebar && node.bounds.x + node.bounds.width >= SIDEBAR_RIGHT - 16.0 {
                node.bounds.width += widened;
            }
            let hidden_by_drawer = self.drawer_open
                && matches!(area, Area::Tabs | Area::Editor | Area::Results)
                && node.bounds.x + self.centre_shift() < splitters::SIDEBAR_DEFAULT;
            let placed = match area {
                _ if hidden_by_drawer => None,
                Area::Sidebar if self.drawer_open => Some(Placed {
                    shift: (0.0, 0.0),
                    clip: Rect::new(
                        0.0,
                        TOOLBAR_HEIGHT,
                        splitters::SIDEBAR_DEFAULT,
                        BODY_BOTTOM - TOOLBAR_HEIGHT,
                    ),
                }),
                _ => self.placed(area),
            };
            match placed {
                Some(placed) => {
                    node.bounds.x += placed.shift.0;
                    node.bounds.y += placed.shift.1;
                    let right =
                        (node.bounds.x + node.bounds.width).min(placed.clip.x + placed.clip.w);
                    let bottom =
                        (node.bounds.y + node.bounds.height).min(placed.clip.y + placed.clip.h);
                    node.bounds.width = (right - node.bounds.x).max(0.0);
                    node.bounds.height = (bottom - node.bounds.y).max(0.0);
                }
                None => {
                    node.bounds.width = 0.0;
                    node.bounds.height = 0.0;
                }
            }
        }
    }

    /// Opens the layout picker over the window, the current layout highlighted.
    pub fn open_layout_picker(&mut self) {
        self.layout_picker = Some(LayoutPicker::opened(self.layout));
        self.chord_started = false;
    }

    /// Answers the picker's outcome: it closes, and an applied layout takes effect at once,
    /// keeping every tab, caret, result and the connection.
    pub fn picker_outcome(&mut self, outcome: PickerOutcome) -> WindowAction {
        match outcome {
            PickerOutcome::Open => WindowAction::Tab(TabAction::None),
            PickerOutcome::Keep => {
                self.layout_picker = None;
                WindowAction::Tab(TabAction::None)
            }
            PickerOutcome::Apply(layout) => {
                self.layout_picker = None;
                self.layout = layout;
                WindowAction::LayoutChosen(layout)
            }
        }
    }

    /// The editor position a press at (`x`, `y`) logical pixels reaches: the visible line
    /// under it (the first or last one above or below the text) and the nearest character
    /// boundary on it.
    pub fn editor_position_at(&self, faces: Faces<'_>, x: f32, y: f32) -> Position {
        let shift = self
            .placed(Area::Editor)
            .map_or((0.0, 0.0), |placed| placed.shift);
        let (x, y) = (x - shift.0, y - shift.1);
        let tab = &self.tab;
        let lines = tab.editor.lines();
        let visible =
            (((editor_bottom(tab) - EDITOR_TEXT_TOP) / LINE_HEIGHT).floor() as usize).max(1);
        let first =
            first_visible_line(tab.editor.caret().line, visible).min(lines.len().saturating_sub(1));
        let row = ((y - EDITOR_TEXT_TOP) / LINE_HEIGHT).floor().max(0.0) as usize;
        let line = (first + row.min(visible - 1)).min(lines.len().saturating_sub(1));
        let text = lines.get(line).map_or("", String::as_str);
        let column = pointer::column_at(faces.mono, CODE.0, text, x - EDITOR_TEXT_LEFT);
        Position { line, column }
    }

    /// Answers a press on the editor at (`x`, `y`): the caret at the press (the selection
    /// extended to it with `extend`), the word on a double-click, the line on a triple-click.
    pub fn press_editor(&mut self, faces: Faces<'_>, x: f32, y: f32, clicks: u8, extend: bool) {
        let at = self.editor_position_at(faces, x, y);
        let editor = &mut self.tab.editor;
        match clicks {
            2 => {
                let line = editor.lines().get(at.line).map_or("", String::as_str);
                let (start, end) = pointer::word_at(line, at.column);
                editor.select_columns(at.line, start, end);
            }
            3 => editor.select_line(at.line),
            _ if extend => editor.select_to(at),
            _ => editor.place_caret(at),
        }
    }

    /// Extends the editor's selection to (`x`, `y`) during a drag.
    pub fn drag_editor(&mut self, faces: Faces<'_>, x: f32, y: f32) {
        let at = self.editor_position_at(faces, x, y);
        self.tab.editor.select_to(at);
    }

    /// Scrolls what is under (`x`, `y`) by the wheel's `lines_down` and `lines_right`, one line
    /// a notch on every platform (`Shift` turns vertical steps horizontal): the grid by
    /// [`pointer::WHEEL_ROWS_PER_NOTCH`] rows or columns a notch, the object tree by as many
    /// rows; parts of a step add up over events. Returns whether anything moved.
    pub fn wheel(
        &mut self,
        tree: &[AccessNode],
        x: f32,
        y: f32,
        lines_down: f32,
        lines_right: f32,
        shift: bool,
    ) -> bool {
        let over = |id: u32| {
            tree.iter()
                .find(|node| node.id == id)
                .is_some_and(|node| pointer::contains(node.bounds, x, y))
        };
        let (down, right) = if shift {
            (0.0, lines_right + lines_down)
        } else {
            (lines_down, lines_right)
        };
        if over(NODE_QUERIES) {
            let notches = if down != 0.0 { down } else { right };
            return notches.is_finite() && self.scroll_tabs(notches);
        }
        let (down, right) = wheel_steps(&mut self.wheel_carry, down, right);
        if let Some(rect) = self.value_view_rect()
            && rect_contains(rect, x, y)
            && let Some(view) = &mut self.tab.value_view
        {
            let before = view.first_line;
            view.scroll_by(down);
            return view.first_line != before;
        }
        if over(NODE_GRID) {
            if let Some(grid) = self
                .tab
                .grid
                .as_mut()
                .filter(|_| self.tab.view == ResultView::Results)
            {
                let before = (grid.first_row, grid.scroll_x);
                grid.scroll_by(down, right, crate::query_tab::ROWS_IN_VIEW);
                return (grid.first_row, grid.scroll_x) != before;
            }
            return false;
        }
        if over(NODE_OBJECTS) {
            let before = self.tree.first_row;
            let last = self
                .tree
                .rows()
                .len()
                .saturating_sub(object_browser::ROWS_IN_VIEW);
            self.tree.first_row = before.saturating_add_signed(down).min(last);
            return self.tree.first_row != before;
        }
        false
    }

    /// The sample of `docs/ux/gui.md`'s golden frame: the `shop` database.
    pub fn sample() -> Self {
        let mut tab = QueryTab::with_text("Query 1", "SELECT id, name FROM customers ORDER BY id;");
        tab.session = SessionId(1);
        let rows = [
            ("1", "Ada Lovelace"),
            ("2", "Grace Hopper"),
            ("3", "Alan Turing"),
        ]
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
        let width = width.max(crate::connect_screen::MIN_WIDTH as usize);
        let wide = |width: usize| width >= inspector::DOCKED_FROM_WIDTH;
        if wide(width) && !wide(self.width) {
            self.inspector.docked = self.sizes.inspector_shown || self.inspector.overlay;
            self.inspector.overlay = false;
        } else if !wide(width) && wide(self.width) {
            self.inspector.docked = false;
        }
        self.width = width;
    }

    /// Applies sizes read from the settings: a wide window docks the inspector only when
    /// they keep it shown.
    pub fn use_sizes(&mut self, sizes: RegionSizes) {
        self.sizes = sizes;
        if self.width >= inspector::DOCKED_FROM_WIDTH {
            self.inspector.docked = sizes.inspector_shown;
        }
    }

    /// The right edge of the main area: the window's, or the docked inspector's left edge.
    pub fn centre_right(&self) -> f32 {
        let docked = self.docked_width();
        if self.layout == Layout::EggDb {
            return self.width as f32 - docked - self.centre_shift();
        }
        let editor = self.layout.regions_at(self.width as f32).editor;
        SIDEBAR_RIGHT + editor.w - docked
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
        if let Some(picker) = &mut self.layout_picker {
            let outcome = picker.key(press);
            return self.picker_outcome(outcome);
        }
        if let Some(prompt) = &mut self.close_prompt {
            let answer = prompt.key(press);
            return self.prompt_answer(answer);
        }
        if let Some((prompt, _)) = &mut self.drop_prompt {
            let answer = prompt.key(press);
            return self.drop_answer(answer);
        }
        if let Some(menu) = &mut self.tree_menu {
            let answer = menu.key(press);
            return self.tree_menu_answer(answer, now);
        }
        if self.tab.value_view.is_some()
            || self.tab.grid_menu.is_some()
            || self.tab.editor_menu.is_some()
        {
            return WindowAction::Tab(self.tab.key(press));
        }
        if self.inspector.ddl_focused {
            return self.ddl_key(press);
        }
        if let Some(splitter) = self.focused_splitter {
            return self.splitter_key(splitter, press);
        }
        if primary && !press.modifiers.shift && matches!(press.key, Key::Character('b' | 'B')) {
            self.toggle_sidebar();
            return WindowAction::Tab(TabAction::None);
        }
        if self.drawer_open && press.key == Key::Escape {
            self.close_drawer();
            return WindowAction::Tab(TabAction::None);
        }
        if self.focus == WindowFocus::Tab
            && let Some(action) = self.tab.keys_tab_key(press, now)
        {
            return WindowAction::Tab(action);
        }
        if let Some(action) = self.tab_key(press) {
            return action;
        }
        if std::mem::take(&mut self.chord_started)
            && matches!(press.key, Key::Character('l' | 'L'))
            && !primary
        {
            self.open_layout_picker();
            return WindowAction::Tab(TabAction::None);
        }
        if primary && !press.modifiers.shift && matches!(press.key, Key::Character('k' | 'K')) {
            self.chord_started = true;
            return WindowAction::Tab(TabAction::None);
        }
        let press = if self.focus == WindowFocus::Tab
            && self.tab.focus == TabFocus::Editor
            && self.tab.dialect_menu.is_none()
        {
            self.layout.translate(press)
        } else {
            press
        };
        match press.key {
            Key::Character('1') if primary => {
                if self.rail_shown() {
                    self.open_drawer(RailItem::Objects);
                } else {
                    self.focus_tree();
                }
                return WindowAction::Tab(TabAction::None);
            }
            Key::Character('i' | 'I') if primary && !press.modifiers.shift => {
                self.inspector.toggle(self.width);
                if self.width >= inspector::DOCKED_FROM_WIDTH {
                    self.sizes.inspector_shown = self.inspector.docked;
                    self.sizes_changed = true;
                }
                return self.inspect_selected();
            }
            Key::Escape if self.inspector.overlay && self.focus != WindowFocus::Filter => {
                self.inspector.overlay = false;
                return WindowAction::Tab(TabAction::None);
            }
            Key::F10 | Key::ContextMenu
                if self.focus == WindowFocus::Tree
                    && (press.key == Key::ContextMenu || press.modifiers.shift) =>
            {
                self.open_tree_menu(self.tree.current, TreeMenuOrigin::CurrentRow);
                return WindowAction::Tab(TabAction::None);
            }
            Key::F10 | Key::ContextMenu
                if self.focus == WindowFocus::Tab
                    && self.tab.focus == TabFocus::Editor
                    && (press.key == Key::ContextMenu || press.modifiers.shift) =>
            {
                self.open_editor_menu_at_caret();
                return WindowAction::Tab(TabAction::None);
            }
            Key::Character('c' | 'C')
                if primary && !press.modifiers.shift && self.focus == WindowFocus::Tree =>
            {
                return self.copy_name(self.tree.current);
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
        self.inspector.ddl_focused = false;
        self.focused_splitter = None;
        self.focus = WindowFocus::Tree;
        self.tab.dialect_menu = None;
        self.tab.focus = TabFocus::Editor;
        if let Some(grid) = &mut self.tab.grid {
            grid.focused = false;
        }
    }

    fn focus_tab(&mut self) {
        self.inspector.ddl_focused = false;
        self.focused_splitter = None;
        self.focus = WindowFocus::Tab;
        self.tab.focus = TabFocus::Editor;
    }

    fn tree_action(&mut self, action: TreeAction, from: WindowFocus) -> WindowAction {
        let none = WindowAction::Tab(TabAction::None);
        match action {
            TreeAction::None => none,
            TreeAction::Open(item) => {
                let action = self.open_object(item);
                self.drawer_open = false;
                self.drawer_from = None;
                action
            }
            TreeAction::InsertName(item) => {
                if let Some(name) = self.tree.name_of(item).filter(|_| !self.tab.is_keys()) {
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
                if self.splitter_band(Splitter::Sidebar).is_some() && !self.drawer_open {
                    self.focused_splitter = Some(Splitter::Sidebar);
                }
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
        if let TreeItem::KeySpace(space) = item {
            return self.open_keys(space);
        }
        let Some(name) = self.tree.name_of(item) else {
            return WindowAction::Tab(TabAction::None);
        };
        if self.tab.running {
            self.tab.notice = Some(OPEN_WHILE_RUNNING.to_owned());
            return WindowAction::Tab(TabAction::None);
        }
        let dialect = self.nearest_query_dialect();
        if !self.new_tab() {
            return WindowAction::Tab(TabAction::None);
        }
        self.tab.editor = crate::editor::Editor::new(&open_statement(&name, dialect));
        match self.tab.run_request(true) {
            Some(request) => WindowAction::Tab(TabAction::Run(Box::new(request))),
            None => WindowAction::Tab(TabAction::None),
        }
    }

    /// `Enter` on a key space (`docs/ux/workspace.md` 10.4): shows its keys tab, opening one
    /// when it has none, and scans the space again from the start.
    pub fn open_keys(&mut self, space: usize) -> WindowAction {
        let none = WindowAction::Tab(TabAction::None);
        let Some(key_space) = self.tree.catalog.key_spaces.get(space) else {
            return none;
        };
        if self.tab.running {
            self.tab.notice = Some(OPEN_WHILE_RUNNING.to_owned());
            return none;
        }
        let scan = KeySpaceScan {
            over_resp: self.tree.catalog.over_resp,
            ..KeySpaceScan::new(key_space.collection.clone(), key_space.keys)
        };
        let open = (0..self.tab_count()).find(|&position| {
            self.tab_at(position)
                .and_then(|tab| tab.keys.as_ref())
                .is_some_and(|keys| keys.collection == scan.collection)
        });
        match open {
            Some(position) => {
                self.show_tab(position);
                if let Some(keys) = &mut self.tab.keys {
                    keys.total = scan.total;
                }
            }
            None => {
                let session = self.take_session();
                let tab = QueryTab {
                    session,
                    ..QueryTab::keys(scan)
                };
                if !self.insert_tab(tab) {
                    return none;
                }
            }
        }
        self.tab.run_request(false).map_or(none, |request| {
            WindowAction::Tab(TabAction::Run(Box::new(request)))
        })
    }

    /// A RESP server's window opens on its first key space's keys tab in place of the empty
    /// `Query 1`, which can run nothing there; it returns the scan to run, or `None` when the
    /// window is not over RESP, already has other tabs, or the server lists no key space.
    pub fn open_on_first_key_space(&mut self) -> Option<WindowAction> {
        if !self.tree.catalog.over_resp || self.tab_count() != 1 || self.tab.is_keys() {
            return None;
        }
        let key_space = self.tree.catalog.key_spaces.first()?;
        let scan = KeySpaceScan {
            over_resp: true,
            ..KeySpaceScan::new(key_space.collection.clone(), key_space.keys)
        };
        let session = self.tab.session;
        self.tab = QueryTab {
            session,
            ..QueryTab::keys(scan)
        };
        self.tab
            .run_request(false)
            .map(|request| WindowAction::Tab(TabAction::Run(Box::new(request))))
    }

    /// The dialect a new query tab starts in: the showing tab's, or from a keys tab the
    /// nearest query tab's to its left, else to its right, else PostgreSQL's.
    pub fn nearest_query_dialect(&self) -> QueryDialect {
        if !self.tab.is_keys() {
            return self.tab.dialect;
        }
        (0..self.tab_position)
            .rev()
            .chain(self.tab_position + 1..self.tab_count())
            .filter_map(|position| self.tab_at(position))
            .find(|tab| !tab.is_keys())
            .map_or(QueryDialect::PostgreSql, |tab| tab.dialect)
    }
}

impl MainWindow {
    /// How many tabs the strip has.
    pub fn tab_count(&self) -> usize {
        self.background_tabs.len() + 1
    }

    /// The tabs' titles in the strip's order.
    pub fn tab_titles(&self) -> Vec<&str> {
        let mut titles: Vec<&str> = self
            .background_tabs
            .iter()
            .map(|tab| tab.title.as_str())
            .collect();
        titles.insert(self.tab_position.min(titles.len()), self.tab.title.as_str());
        titles
    }

    /// The tab in the strip at `position`.
    pub fn tab_at(&self, position: usize) -> Option<&QueryTab> {
        match position.cmp(&self.tab_position) {
            std::cmp::Ordering::Less => self.background_tabs.get(position),
            std::cmp::Ordering::Equal => Some(&self.tab),
            std::cmp::Ordering::Greater => self.background_tabs.get(position - 1),
        }
    }

    /// The tab whose kept session is `session`, showing or not.
    pub fn tab_with_session_mut(&mut self, session: SessionId) -> Option<&mut QueryTab> {
        if self.tab.session == session {
            return Some(&mut self.tab);
        }
        self.background_tabs
            .iter_mut()
            .find(|tab| tab.session == session)
    }

    /// The tab, other than the showing one, whose session has a transaction open or failed.
    pub fn other_tab_in_transaction(&self) -> Option<&QueryTab> {
        self.background_tabs
            .iter()
            .find(|tab| tab.transaction != eggdb::sql::TransactionStatus::Idle)
    }

    fn strip_width(&self) -> f32 {
        match self.placed(Area::Tabs) {
            Some(placed) if self.layout != Layout::EggDb => placed.clip.w,
            _ => self.centre_right() - 260.0,
        }
    }

    fn take_session(&mut self) -> SessionId {
        let session = self.next_session;
        self.next_session = SessionId(session.0 + 1);
        session
    }

    fn fresh_tab(&mut self, title: &str) -> QueryTab {
        let session = self.take_session();
        QueryTab {
            session,
            ..QueryTab::new(title)
        }
    }

    fn take_strip(&mut self) -> Vec<QueryTab> {
        let shown = std::mem::replace(&mut self.tab, QueryTab::new(""));
        let mut strip = std::mem::take(&mut self.background_tabs);
        strip.insert(self.tab_position.min(strip.len()), shown);
        strip
    }

    fn show_from(&mut self, mut strip: Vec<QueryTab>, position: usize) {
        if strip.is_empty() {
            let first = self.fresh_tab("Query 1");
            strip.push(first);
        }
        let position = position.min(strip.len() - 1);
        self.tab = strip.remove(position);
        self.background_tabs = strip;
        self.tab_position = position;
        self.tab_scroll = tab_strip::scroll_to_show(
            self.tab_scroll,
            position,
            self.tab_count(),
            self.strip_width(),
        );
    }

    /// `Ctrl/Cmd+T` (`docs/ux/workspace.md` 9.1): `Query n`, the lowest unused number, right
    /// after the showing tab with its dialect, shown with the focus in its empty editor.
    pub fn add_tab(&mut self) {
        if self.new_tab() {
            self.focus_tab();
        }
    }

    fn new_tab(&mut self) -> bool {
        if self.tree.catalog.over_resp {
            self.tab.notice = Some(crate::connection::QUERY_TABS_NEED_SQL.to_owned());
            return false;
        }
        if self.tab_count() >= tab_strip::MOST_TABS {
            self.tab.notice = Some(tab_strip::TOO_MANY_TABS.to_owned());
            return false;
        }
        let title = tab_strip::lowest_free_title(self.tab_titles().into_iter());
        let dialect = self.nearest_query_dialect();
        let mut tab = self.fresh_tab(&title);
        tab.set_dialect(dialect);
        self.insert_tab(tab)
    }

    fn insert_tab(&mut self, tab: QueryTab) -> bool {
        if self.tab_count() >= tab_strip::MOST_TABS {
            self.tab.notice = Some(tab_strip::TOO_MANY_TABS.to_owned());
            return false;
        }
        let position = self.tab_position + 1;
        let mut strip = self.take_strip();
        strip.insert(position.min(strip.len()), tab);
        self.show_from(strip, position);
        true
    }

    /// Shows the tab at `position` in the strip.
    pub fn show_tab(&mut self, position: usize) {
        if position == self.tab_position || position >= self.tab_count() {
            return;
        }
        let strip = self.take_strip();
        self.show_from(strip, position);
        self.focus_tab();
    }

    /// `Ctrl+Tab` (or with `back`, `Ctrl+Shift+Tab`): the next (previous) tab, wrapping.
    pub fn cycle_tab(&mut self, back: bool) {
        let count = self.tab_count();
        let position = if back {
            (self.tab_position + count - 1) % count
        } else {
            (self.tab_position + 1) % count
        };
        self.show_tab(position);
    }

    /// `Ctrl/Cmd+W`: closes the showing tab, first asking when it runs, has a transaction
    /// open or holds text (`docs/ux/workspace.md` 9.1).
    pub fn close_tab(&mut self) -> WindowAction {
        match ClosePrompt::for_tab(&self.tab) {
            Some(prompt) => {
                self.tab.dialect_menu = None;
                self.close_prompt = Some(prompt);
                WindowAction::Tab(TabAction::None)
            }
            None => self.close_shown_tab(),
        }
    }

    /// Closes the tab at `position`, showing it first so its dialog, if any, is about it.
    pub fn close_tab_at(&mut self, position: usize) -> WindowAction {
        self.show_tab(position);
        self.close_tab()
    }

    fn close_shown_tab(&mut self) -> WindowAction {
        let position = self.tab_position;
        let mut strip = self.take_strip();
        let closed = strip.remove(position.min(strip.len() - 1));
        let next = if position < strip.len() {
            position
        } else {
            position.saturating_sub(1)
        };
        self.show_from(strip, next);
        self.focus_tab();
        WindowAction::CloseSession {
            session: closed.session,
            stop: closed.running,
        }
    }

    fn prompt_answer(&mut self, answer: PromptAnswer) -> WindowAction {
        match answer {
            PromptAnswer::Pending => WindowAction::Tab(TabAction::None),
            PromptAnswer::Keep => {
                self.close_prompt = None;
                WindowAction::Tab(TabAction::None)
            }
            PromptAnswer::Close => {
                self.close_prompt = None;
                self.close_shown_tab()
            }
        }
    }

    fn tab_key(&mut self, press: KeyPress) -> Option<WindowAction> {
        let primary = press.modifiers.control || press.modifiers.command;
        let none = WindowAction::Tab(TabAction::None);
        match press.key {
            Key::Character('t' | 'T') if primary && !press.modifiers.shift => {
                self.add_tab();
                Some(none)
            }
            Key::Character('w' | 'W') if primary && !press.modifiers.shift => {
                Some(self.close_tab())
            }
            Key::F4 if press.modifiers.control => Some(self.close_tab()),
            Key::Tab if press.modifiers.control => {
                self.cycle_tab(press.modifiers.shift);
                Some(none)
            }
            _ => None,
        }
    }

    /// The wheel over the tab strip: 100 px a notch on either axis, while the tabs overflow.
    pub fn scroll_tabs(&mut self, notches: f32) -> bool {
        let max = tab_strip::max_scroll(self.tab_count(), self.strip_width());
        let scrolled = (self.tab_scroll + notches * tab_strip::WHEEL_STEP).clamp(0.0, max);
        let changed = scrolled != self.tab_scroll;
        self.tab_scroll = scrolled;
        changed
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

/// The whole rows (or columns) down and right that `lines_down` and `lines_right` scroll at
/// [`pointer::WHEEL_ROWS_PER_NOTCH`] a line, with `carry`'s parts of a row from earlier events
/// added and the new parts left in it.
pub fn wheel_steps(carry: &mut (f32, f32), lines_down: f32, lines_right: f32) -> (isize, isize) {
    let finite = |lines: f32| if lines.is_finite() { lines } else { 0.0 };
    let down = carry.0 + finite(lines_down) * pointer::WHEEL_ROWS_PER_NOTCH;
    let right = carry.1 + finite(lines_right) * pointer::WHEEL_ROWS_PER_NOTCH;
    *carry = (down.fract(), right.fract());
    (down.trunc() as isize, right.trunc() as isize)
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
        triggers: Vec::new(),
        over_resp: false,
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
            Button::PrimaryHovered => self.canvas.fill_rounded_rect(rect, 4.0, theme.accent_hover),
            Button::PrimaryPressed => {
                self.canvas
                    .fill_rounded_rect(rect, 4.0, theme.accent_pressed)
            }
            Button::PrimaryFading(fill) => {
                let color = fill.blend(theme.accent, theme.accent_hover, theme.accent_pressed);
                self.canvas.fill_rounded_rect(rect, 4.0, color);
            }
            Button::SecondaryFading { fill, pressed } => {
                let edge = if pressed {
                    theme.border_strong
                } else {
                    theme.border
                };
                self.canvas.fill_rounded_rect(rect, 4.0, edge);
                let inner = Rect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, rect.h - 2.0);
                let color = fill.blend(theme.bg0, theme.bg2, theme.bg2);
                self.canvas.fill_rounded_rect(inner, 3.0, color);
            }
            Button::SecondaryHovered | Button::SecondaryPressed => {
                let edge = if kind == Button::SecondaryPressed {
                    theme.border_strong
                } else {
                    theme.border
                };
                self.canvas.fill_rounded_rect(rect, 4.0, edge);
                let inner = Rect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, rect.h - 2.0);
                self.canvas.fill_rounded_rect(inner, 3.0, theme.bg2);
            }
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
    PrimaryHovered,
    PrimaryPressed,
    Secondary,
    SecondaryHovered,
    SecondaryPressed,
    PrimaryFading(FillMix),
    SecondaryFading { fill: FillMix, pressed: bool },
    Disabled,
    Ghost,
}

fn under_pointer(window: &MainWindow, id: u32, look: Button) -> Button {
    let pointer = window.pointer;
    let fill = pointer.mix(id);
    let fading = !fill.is(pointer.fill(id));
    match look {
        Button::Primary if fading => Button::PrimaryFading(fill),
        Button::Secondary if fading => Button::SecondaryFading {
            fill,
            pressed: pointer.fill(id) == Fill::Pressed,
        },
        Button::Primary if pointer.is_pressed(id) => Button::PrimaryPressed,
        Button::Primary if pointer.is_hovered(id) => Button::PrimaryHovered,
        Button::Secondary if pointer.is_pressed(id) => Button::SecondaryPressed,
        Button::Secondary if pointer.is_hovered(id) => Button::SecondaryHovered,
        other => other,
    }
}

/// Draws `window` into a new canvas of its width and [`HEIGHT`] in `theme`, as
/// `docs/ux/gui.md` and `docs/ux/workspace.md` lay it out.
pub fn draw(window: &MainWindow, theme: Theme, faces: Faces<'_>) -> Canvas {
    let mut canvas = Canvas::new(window.width, HEIGHT, theme.bg0);
    let mut paint = Painter {
        canvas: &mut canvas,
        theme,
        faces,
        width: window.width as f32,
        centre_right: window.centre_right(),
    };
    if window.layout == Layout::EggDb && window.sizes_moved() {
        toolbar(&mut paint, window);
        sized_regions(&mut paint, window);
    } else if window.layout == Layout::EggDb {
        toolbar(&mut paint, window);
        object_browser::draw(
            &mut paint,
            &window.tree,
            access_text(window),
            sidebar_focus(window),
            window.sizes.sidebar_width,
        );
        tabs(&mut paint, window);
        editor(&mut paint, &window.tab, window.caret);
        results(&mut paint, window);
        splitter_marks(&mut paint, window);
    } else {
        placed_regions(&mut paint, window);
    }
    if window.inspector.is_open() {
        let description = inspector::describe(
            &window.tree,
            window.inspected(),
            window.inspector.sample.as_ref(),
        );
        inspector::draw(
            &mut paint,
            &window.inspector,
            &description,
            window.sizes.inspector_width,
        );
    }
    if window.layout == Layout::EggDb {
        status_bar(&mut paint, window);
        dialect_menu(&mut paint, &window.tab);
    } else {
        if let Some(placed) = window.placed(Area::StatusBar) {
            paint.width = placed.clip.w;
            place(paint.canvas, placed);
            status_bar(&mut paint, window);
            paint.canvas.draw_anywhere();
        }
        if let Some(placed) = window.placed(Area::Toolbar) {
            paint.width = placed.clip.w;
            let anywhere = Placed {
                shift: placed.shift,
                clip: Rect::new(0.0, 0.0, window.width as f32, HEIGHT as f32),
            };
            place(paint.canvas, anywhere);
            dialect_menu(&mut paint, &window.tab);
            paint.canvas.draw_anywhere();
        }
    }
    popovers(&mut paint, window);
    if let Some(prompt) = &window.close_prompt {
        let size = (window.width as f32, HEIGHT as f32);
        tab_strip::draw_prompt(&mut paint, prompt, size);
    }
    if let Some((prompt, _)) = &window.drop_prompt {
        let size = (window.width as f32, HEIGHT as f32);
        drop_prompt::draw(&mut paint, prompt, size);
    }
    if let Some(picker) = &window.layout_picker {
        let size = (window.width as f32, HEIGHT as f32);
        layout_picker::draw(&mut canvas, picker, theme, faces, size, 1.0);
    }
    canvas
}

fn sized_regions(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let theme = paint.theme;
    if window.rail_shown() {
        objects_rail(paint, window);
    } else {
        object_browser::draw(
            paint,
            &window.tree,
            access_text(window),
            sidebar_focus(window),
            window.sizes.sidebar_width,
        );
    }
    for area in [Area::Tabs, Area::Editor, Area::Results] {
        let Some(placed) = window.placed(area) else {
            continue;
        };
        let clip = placed.clip;
        paint.rect(clip.x, clip.y, clip.w, clip.h, theme.bg0);
        place(paint.canvas, placed);
        match area {
            Area::Tabs => tabs(paint, window),
            Area::Editor => editor(paint, &window.tab, window.caret),
            _ => results(paint, window),
        }
        paint.canvas.draw_anywhere();
    }
    splitter_marks(paint, window);
    if window.drawer_open {
        drawer(paint, window);
    }
}

fn splitter_marks(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let theme = paint.theme;
    for splitter in [Splitter::Sidebar, Splitter::Results, Splitter::Inspector] {
        let Some(band) = window.splitter_band(splitter) else {
            continue;
        };
        let focused = window.focused_splitter == Some(splitter);
        let active = focused
            || window.hot_splitter == Some(splitter)
            || window.dragged_splitter == Some(splitter);
        if !active {
            continue;
        }
        if splitter.is_vertical() {
            paint.rect(band.x + 2.0, band.y, 2.0, band.h, theme.accent);
        } else {
            paint.rect(band.x, band.y + 2.0, band.w, 2.0, theme.accent);
        }
        if focused {
            let ring = if splitter.is_vertical() {
                Rect::new(band.x - 2.0, band.y, band.w + 4.0, band.h)
            } else {
                Rect::new(band.x, band.y - 2.0, band.w, band.h + 4.0)
            };
            focus_ring(paint, ring);
        }
    }
}

fn objects_rail(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let theme = paint.theme;
    let body = BODY_BOTTOM - TOOLBAR_HEIGHT;
    paint.rect(0.0, TOOLBAR_HEIGHT, RAIL_WIDTH, body, theme.bg1);
    paint.rect(RAIL_WIDTH - 1.0, TOOLBAR_HEIGHT, 1.0, body, theme.border);
    for (index, item) in window.rail_items().into_iter().enumerate() {
        let top = MainWindow::rail_item_top(index);
        if window.drawer_from == Some(item) {
            paint.rect(0.0, top, RAIL_WIDTH, RAIL_BUTTON_HEIGHT, theme.selection);
            paint.rect(0.0, top, 2.0, RAIL_BUTTON_HEIGHT, theme.accent);
        }
        let (x, y) = (12.0, top + 10.0);
        match item {
            RailItem::Objects => object_browser::draw_icon(paint, RowIcon::Database, x, y),
            RailItem::Filter => {
                paint
                    .canvas
                    .stroke_circle(x + 5.0, y + 5.0, 8.0, 1.5, theme.text_secondary);
                paint.canvas.line(
                    (x + 8.5, y + 8.5),
                    (x + 12.0, y + 12.0),
                    1.5,
                    theme.text_secondary,
                );
            }
            RailItem::Group(Group::Keys) => object_browser::draw_icon(paint, RowIcon::Key, x, y),
            RailItem::Group(_) => object_browser::draw_icon(paint, RowIcon::Table, x, y),
        }
        if index == 1 {
            let separator = top + RAIL_BUTTON_HEIGHT + 4.0;
            paint.rect(8.0, separator, 20.0, 1.0, theme.border);
        }
    }
}

fn drawer(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let width = splitters::SIDEBAR_DEFAULT;
    for step in 0..12 {
        let alpha = (41 * (step + 1) / 12) as u8;
        paint.rect(
            width + 11.0 - step as f32,
            TOOLBAR_HEIGHT + 4.0,
            1.0,
            BODY_BOTTOM - TOOLBAR_HEIGHT - 4.0,
            Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: alpha / 3,
            },
        );
    }
    object_browser::draw(
        paint,
        &window.tree,
        access_text(window),
        sidebar_focus(window),
        width,
    );
}

fn sidebar_focus(window: &MainWindow) -> SidebarFocus {
    match window.focus {
        WindowFocus::Tab => SidebarFocus::None,
        WindowFocus::Tree => SidebarFocus::Tree,
        WindowFocus::Filter => SidebarFocus::Filter,
    }
}

fn place(canvas: &mut Canvas, placed: Placed) {
    canvas.draw_placed(
        (placed.shift.0.round() as i64, placed.shift.1.round() as i64),
        placed.clip,
    );
}

fn placed_regions(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let full_width = paint.width;
    let theme = paint.theme;
    for area in AREAS {
        let Some(placed) = window.placed(area) else {
            continue;
        };
        match area {
            Area::Toolbar => {
                paint.width = placed.clip.w;
                place(paint.canvas, placed);
                toolbar(paint, window);
            }
            Area::Sidebar => {
                let target = placed.clip;
                paint.rect(target.x, target.y, target.w, target.h, theme.bg1);
                paint.rect(
                    target.x + target.w - 1.0,
                    target.y,
                    1.0,
                    target.h,
                    theme.border,
                );
                if target.w < SIDEBAR_RIGHT {
                    rail(paint, target);
                    continue;
                }
                let inside = if target.w > SIDEBAR_RIGHT {
                    SIDEBAR_RIGHT - 1.0
                } else {
                    target.w
                };
                let clip = Rect::new(target.x, target.y, inside, target.h);
                place(paint.canvas, Placed { clip, ..placed });
                object_browser::draw(
                    paint,
                    &window.tree,
                    access_text(window),
                    sidebar_focus(window),
                    object_browser::SIDEBAR_WIDTH,
                );
            }
            Area::Tabs => {
                place(paint.canvas, placed);
                tabs(paint, window);
            }
            Area::Editor => {
                place(paint.canvas, placed);
                editor(paint, &window.tab, window.caret);
            }
            Area::Results => {
                place(paint.canvas, placed);
                splitter(paint);
                results(paint, window);
            }
            Area::StatusBar => continue,
        }
        paint.canvas.draw_anywhere();
        paint.width = full_width;
    }
    paint.width = full_width;
    let regions = window.layout.regions_at(full_width);
    if let Some((name, rect)) = regions.extra {
        extra_region(paint, name, rect);
    }
}

const RAIL_ITEMS: [&str; 4] = ["Browser", "Workbench", "Analysis", "Pub/Sub"];
const RAIL_ITEM_HEIGHT: f32 = 64.0;
const RAIL_SELECTED: usize = 1;

fn rail(paint: &mut Painter<'_, '_>, target: Rect) {
    let theme = paint.theme;
    let faces = paint.faces;
    for (index, label) in RAIL_ITEMS.into_iter().enumerate() {
        let top = target.y + RAIL_ITEM_HEIGHT * index as f32;
        if index == RAIL_SELECTED {
            paint.canvas.fill_rounded_rect(
                Rect::new(
                    target.x + 4.0,
                    top + 4.0,
                    target.w - 9.0,
                    RAIL_ITEM_HEIGHT - 8.0,
                ),
                4.0,
                theme.selection,
            );
        }
        let width = faces.ui.width(label, SMALL.0);
        let left = target.x + ((target.w - 1.0 - width) / 2.0).max(2.0);
        let ink = if index == RAIL_SELECTED {
            theme.text_primary
        } else {
            theme.text_secondary
        };
        let shown = elided(faces.ui, label, target.w - 5.0, SMALL.0);
        paint.text(faces.ui, &shown, left, top + 25.0, SMALL, ink);
    }
}

fn extra_region(paint: &mut Painter<'_, '_>, name: &str, rect: Rect) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(rect.x, rect.y, rect.w, rect.h, theme.bg1);
    paint.rect(rect.x, rect.y, rect.w, 1.0, theme.border);
    paint.rect(rect.x + rect.w - 1.0, rect.y, 1.0, rect.h, theme.border);
    let title = extra_title(name);
    let top = rect.y + ((rect.h.min(32.0) - BODY.1) / 2.0).floor();
    paint.text(
        faces.ui_bold,
        &title,
        rect.x + 12.0,
        top,
        BODY,
        theme.text_primary,
    );
}

fn extra_title(name: &str) -> String {
    match name {
        "shell" => ">_MONGOSH".to_owned(),
        _ => {
            let mut characters = name.chars();
            characters.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(characters).collect()
            })
        }
    }
}

fn splitter(paint: &mut Painter<'_, '_>) {
    let theme = paint.theme;
    let centre_width = paint.centre_width();
    paint.rect(SIDEBAR_RIGHT, SPLITTER_TOP, centre_width, 6.0, theme.bg2);
    paint.rect(
        SIDEBAR_RIGHT,
        SPLITTER_TOP + 2.0,
        centre_width,
        1.0,
        theme.border,
    );
}

fn access_text(window: &MainWindow) -> &'static str {
    if window.writable {
        "read-write"
    } else {
        "read-only"
    }
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
    let dx = run_button_extra(window.layout);
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
    paint.button(
        Rect::new(232.0, 8.0, 72.0 + dx, 28.0),
        under_pointer(window, NODE_RUN, run_look),
    );
    paint.polyline(
        &[(244.0, 17.0), (244.0, 27.0), (253.0, 22.0), (244.0, 17.0)],
        1.5,
        run_ink,
    );
    paint.text(
        faces.ui,
        window.layout.run_label(),
        260.0,
        13.0,
        BODY,
        run_ink,
    );
    paint.button(
        Rect::new(312.0 + dx, 8.0, 88.0, 28.0),
        under_pointer(window, NODE_RUN_ALL, other_look),
    );
    paint.polyline(
        &[
            (324.0 + dx, 17.0),
            (324.0 + dx, 27.0),
            (333.0 + dx, 22.0),
            (324.0 + dx, 17.0),
        ],
        1.5,
        other_ink,
    );
    paint.rect(335.0 + dx, 17.0, 2.0, 10.0, other_ink);
    paint.text(faces.ui, "Run all", 342.0 + dx, 13.0, BODY, other_ink);
    paint.button(
        Rect::new(408.0 + dx, 8.0, 84.0, 28.0),
        under_pointer(window, NODE_EXPLAIN, other_look),
    );
    for (x, y, w) in [
        (420.0 + dx, 17.0, 10.0),
        (424.0 + dx, 21.0, 6.0),
        (428.0 + dx, 25.0, 2.0),
    ] {
        paint.rect(x, y, w, 2.0, other_ink);
    }
    paint.rect(421.0 + dx, 19.0, 1.0, 7.0, other_ink);
    paint.text(faces.ui, "Explain", 436.0 + dx, 13.0, BODY, other_ink);
    let (stop_look, stop_icon, stop_ink) = if running {
        (Button::Secondary, theme.error, theme.text_primary)
    } else {
        (Button::Disabled, theme.text_disabled, theme.text_disabled)
    };
    paint.button(
        Rect::new(500.0 + dx, 8.0, 68.0, 28.0),
        under_pointer(window, NODE_STOP, stop_look),
    );
    paint
        .canvas
        .fill_rounded_rect(Rect::new(512.0 + dx, 17.0, 10.0, 10.0), 2.0, stop_icon);
    paint.text(faces.ui, "Stop", 528.0 + dx, 13.0, BODY, stop_ink);
    paint.button(
        Rect::new(width - 224.0, 8.0, 120.0, 28.0),
        under_pointer(window, NODE_DIALECT, Button::Secondary),
    );
    let pill_ink = if window.tab.is_keys() {
        theme.text_disabled
    } else {
        theme.text_primary
    };
    paint.text(
        faces.ui,
        window.tab.pill_label(),
        width - 212.0,
        13.0,
        BODY,
        pill_ink,
    );
    paint.polyline(
        &[
            (width - 124.0, 20.0),
            (width - 120.0, 24.0),
            (width - 116.0, 20.0),
        ],
        1.5,
        theme.text_primary,
    );
    if window.tab.focus == TabFocus::DialectPill && !window.pointer.focus_by_pointer {
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

fn run_button_extra(layout: Layout) -> f32 {
    if layout.run_label().chars().count() > 4 {
        24.0
    } else {
        0.0
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
    let ellipsis = face.advance('…', size);
    let mut width = 0.0;
    let mut longest_with_ellipsis = None;
    for (scanned, (at, character)) in text.char_indices().enumerate() {
        if width + ellipsis <= max_width {
            longest_with_ellipsis = Some(at);
        }
        width += face.advance(character, size);
        let more_than_scanned =
            scanned + 1 >= ELIDE_SCAN_LIMIT && at + character.len_utf8() < text.len();
        if width > max_width || more_than_scanned {
            return longest_with_ellipsis
                .and_then(|end| text.get(..end))
                .map_or_else(String::new, |kept| format!("{kept}…"));
        }
    }
    text.to_owned()
}

fn tab_left(window: &MainWindow, position: usize, width: f32) -> f32 {
    260.0 + width * position as f32 - window.tab_scroll
}

fn tab_dot(tab: &QueryTab, theme: Theme) -> Option<Rgba> {
    match tab.transaction {
        eggdb::sql::TransactionStatus::InBlock => Some(theme.warning),
        eggdb::sql::TransactionStatus::Failed => Some(theme.error),
        eggdb::sql::TransactionStatus::Idle => None,
    }
}

fn tabs(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let theme = paint.theme;
    let faces = paint.faces;
    let centre_width = paint.centre_width();
    paint.rect(260.0, 44.0, centre_width, 36.0, theme.bg2);
    paint.rect(260.0, 79.0, centre_width, 1.0, theme.border);
    let count = window.tab_count();
    let width = tab_strip::tab_width(count, centre_width);
    let viewport = tab_strip::viewport_width(count, centre_width);
    let outer_clip = paint
        .canvas
        .clip_within(Rect::new(260.0, 44.0, viewport, 36.0));
    for position in 0..count {
        let Some(other) = window.tab_at(position) else {
            continue;
        };
        if position == window.tab_position {
            continue;
        }
        let left = tab_left(window, position, width);
        let node = background_tab_node(window, position);
        let hovered = window.pointer.is_hovered(node);
        let fill = window.pointer.mix(node);
        if !fill.is(Fill::Rest) {
            paint.rect(
                left,
                44.0,
                width,
                35.0,
                fill.blend(theme.bg2, theme.bg1, theme.bg1),
            );
        }
        let next_is_background = position + 1 != window.tab_position && position + 1 < count;
        if next_is_background {
            paint.rect(left + width - 1.0, 52.0, 1.0, 20.0, theme.border);
        }
        let label_x = tab_label_x(paint, other, left);
        let title = elided(
            faces.ui,
            &other.title,
            left + width - 32.0 - label_x,
            BODY.0,
        );
        paint.text(faces.ui, &title, label_x, 53.0, BODY, theme.text_secondary);
        if hovered {
            close_icon(paint, left + width - 16.0, theme.text_secondary);
        }
    }
    let tab = &window.tab;
    let left = tab_left(window, window.tab_position, width);
    paint.rect(left, 44.0, width, 36.0, theme.bg0);
    paint.rect(left, 44.0, width, 2.0, theme.accent);
    let label_x = tab_label_x(paint, tab, left);
    let title = elided(faces.ui, &tab.title, left + width - 32.0 - label_x, BODY.0);
    paint.text(faces.ui, &title, label_x, 53.0, BODY, theme.text_primary);
    let close = left + width - 16.0;
    if tab.running {
        paint
            .canvas
            .stroke_circle(close, 62.0, 12.0, 1.5, theme.accent);
        paint.canvas.fill_circle(close, 57.0, 3.0, theme.accent);
    } else {
        close_icon(paint, close, theme.text_secondary);
    }
    paint.canvas.restore_clip(outer_clip);
    if tab_strip::scrolls(count, centre_width) {
        paint.rect(260.0 + viewport, 44.0, 1.0, 35.0, theme.border);
    }
    let plus = tab_strip::plus_x(count, centre_width, 260.0) + 14.0;
    paint.canvas.line(
        (plus - 5.0, 62.0),
        (plus + 5.0, 62.0),
        1.5,
        theme.text_primary,
    );
    paint
        .canvas
        .line((plus, 57.0), (plus, 67.0), 1.5, theme.text_primary);
}

fn tab_label_x(paint: &mut Painter<'_, '_>, tab: &QueryTab, left: f32) -> f32 {
    match tab_dot(tab, paint.theme) {
        Some(color) => {
            paint.canvas.fill_circle(left + 15.0, 62.0, 6.0, color);
            left + 24.0
        }
        None => left + 12.0,
    }
}

fn close_icon(paint: &mut Painter<'_, '_>, centre: f32, color: Rgba) {
    paint
        .canvas
        .line((centre - 4.0, 58.0), (centre + 4.0, 66.0), 1.5, color);
    paint
        .canvas
        .line((centre + 4.0, 58.0), (centre - 4.0, 66.0), 1.5, color);
}

fn close_node(position: usize) -> u32 {
    NODE_FIRST_TAB_CLOSE + (position % tab_strip::MOST_TABS) as u32
}

fn background_tab_node(window: &MainWindow, position: usize) -> u32 {
    let index = if position > window.tab_position {
        position - 1
    } else {
        position
    };
    NODE_FIRST_BACKGROUND_TAB + (index % tab_strip::MOST_TABS) as u32
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

fn editor(paint: &mut Painter<'_, '_>, tab: &QueryTab, caret_look: CaretLook) {
    let caret_look = if tab.editor_menu.is_some() && caret_look == CaretLook::Off {
        CaretLook::On
    } else {
        caret_look
    };
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
        .map(|range| {
            (
                tab.editor.position_at(range.start),
                tab.editor.position_at(range.end),
            )
        });
    let squiggle = tab
        .error
        .as_ref()
        .and_then(|error| error.range.clone())
        .map(|range| {
            (
                tab.editor.position_at(range.start),
                tab.editor.position_at(range.end),
            )
        });
    let mut caret_x = 320.0;
    for (number, line) in lines.iter().enumerate().skip(first).take(visible) {
        let top = EDITOR_TEXT_TOP + LINE_HEIGHT * (number - first) as f32;
        if number == caret.line {
            paint.rect(308.0, top, centre_width - 48.0, LINE_HEIGHT, theme.bg1);
        }
        if let Some((start, end)) =
            plan_highlight.filter(|(start, end)| (start.line..=end.line).contains(&number))
        {
            let from = if number == start.line {
                start.column
            } else {
                0
            };
            let to = if number == end.line {
                end.column
            } else {
                line.chars().count() + 1
            };
            let left = column_x(faces.mono, line, from);
            let right = column_x(faces.mono, line, to).max(left + 4.0);
            paint.rect(left, top, right - left, LINE_HEIGHT, theme.selection);
        }
        if let Some((start, end)) =
            selection.filter(|(start, end)| (start.line..=end.line).contains(&number))
        {
            let from = if number == start.line {
                start.column
            } else {
                0
            };
            let to = if number == end.line {
                end.column
            } else {
                line.chars().count() + 1
            };
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
            pen = paint.text(
                faces.mono,
                &line[range],
                pen,
                top,
                CODE,
                token_color(theme, kind),
            );
        }
        if caret_byte == Some(line.len()) {
            caret_x = pen;
        }
        if let Some((start, end)) =
            squiggle.filter(|(start, end)| (start.line..=end.line).contains(&number))
        {
            let from = if number == start.line {
                start.column
            } else {
                0
            };
            let to = if number == end.line {
                end.column
            } else {
                line.chars().count()
            };
            let left = column_x(faces.mono, line, from);
            let right = column_x(faces.mono, line, to).max(left + 4.0);
            zigzag(paint, left, right, top + LINE_HEIGHT - 2.0);
        }
    }
    if caret_look.shown() && (first..first + visible).contains(&caret.line) {
        let caret_top = EDITOR_TEXT_TOP + LINE_HEIGHT * (caret.line - first) as f32;
        let color = if caret_look == CaretLook::Inactive {
            theme.text_disabled
        } else {
            theme.accent
        };
        paint.rect(caret_x.round(), caret_top, 2.0, LINE_HEIGHT, color);
    }
    let extent = Extent {
        viewport: visible as f32 * LINE_HEIGHT,
        content: lines.len() as f32 * LINE_HEIGHT,
        offset: first as f32 * LINE_HEIGHT,
    };
    let track = scrollbar::track_rect(
        Axis::Vertical,
        EDITOR_TOP,
        bottom - EDITOR_TOP,
        paint.centre_right,
        bottom,
    );
    scrollbar::draw(paint, Axis::Vertical, track, extent, false);
    if let Some(error) = tab.error.as_ref().filter(|error| error.strip_shown) {
        error_strip(paint, &error.strip_text(), error.fix_label().as_deref());
    }
    splitter(paint);
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
    paint
        .canvas
        .stroke_circle(x + 6.0, y + 6.0, 12.0, 1.5, color);
    paint
        .canvas
        .line((x + 4.0, y + 4.0), (x + 8.0, y + 8.0), 1.5, color);
    paint
        .canvas
        .line((x + 8.0, y + 4.0), (x + 4.0, y + 8.0), 1.5, color);
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
        &[
            (x + 6.0, y),
            (x + 12.0, y + 11.0),
            (x, y + 11.0),
            (x + 6.0, y),
        ],
        1.5,
        color,
    );
    paint.rect(x + 5.5, y + 4.0, 1.0, 4.0, color);
}

fn results(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let (tab, layout) = (&window.tab, window.layout);
    let theme = paint.theme;
    let faces = paint.faces;
    let (centre_width, centre_right) = (paint.centre_width(), paint.centre_right);
    paint.rect(260.0, 386.0, centre_width, 32.0, theme.bg1);
    paint.rect(260.0, 417.0, centre_width, 1.0, theme.border);
    let messages = tab.view == ResultView::Messages;
    for (name, view, x, underline) in result_segments(tab, layout) {
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
    if tab.is_keys() {
        let (field, _, _) = keys_header_rects(centre_right);
        let left = summary_x(tab, layout);
        let summary = elided(
            faces.ui,
            &tab.summary,
            field.x - KEYS_HEADER_GAP - left,
            BODY.0,
        );
        paint.text(faces.ui, &summary, left, 393.0, BODY, theme.text_secondary);
        keys_header(paint, tab, centre_right);
    } else {
        paint.text(
            faces.ui,
            &tab.summary,
            summary_x(tab, layout),
            393.0,
            BODY,
            theme.text_secondary,
        );
        paint.text(
            faces.ui,
            "Export…",
            centre_right - 76.0,
            393.0,
            BODY,
            theme.text_primary,
        );
    }
    if let Some(plan) = tab.plan.as_ref().filter(|_| tab.view == ResultView::Plan) {
        plan_body(paint, plan, window.hovered_plan_node());
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
        let banner = grid.banner_text();
        let shown = elided(faces.ui, &banner, centre_right - 140.0 - 292.0, BODY.0);
        paint.text(faces.ui, &shown, 292.0, 423.0, BODY, theme.text_primary);
        if grid.key_scan.is_none() {
            paint.text_right(
                faces.ui,
                "Export…",
                centre_right - 44.0,
                423.0,
                BODY,
                theme.text_primary,
            );
        }
        let close = centre_right - 28.0;
        paint.canvas.line(
            (close, 428.0),
            (close + 8.0, 436.0),
            1.5,
            theme.text_secondary,
        );
        paint.canvas.line(
            (close + 8.0, 428.0),
            (close, 436.0),
            1.5,
            theme.text_secondary,
        );
        top = 446.0;
    }
    grid_body(paint, grid, top);
    grid_handles(paint, grid, top, window);
    if grid.rows.is_empty()
        && let Some(empty) = grid.key_scan.as_ref().and_then(KeySpaceScan::empty_text)
    {
        paint.text(
            faces.ui,
            &empty,
            272.0,
            top + grid::HEADER_HEIGHT + 4.0,
            BODY,
            theme.text_secondary,
        );
    }
}

const KEYS_HEADER_GAP: f32 = 12.0;
const KEYS_FIELD_WIDTH: f32 = 200.0;
const KEYS_BACK_WIDTH: f32 = 72.0;
const KEYS_NEXT_WIDTH: f32 = 92.0;
const KEYS_BUTTON_GAP: f32 = 8.0;

/// The keys tab's `Key prefix` field, `Back` and `Next 100` in the results header
/// (`docs/ux/workspace.md` 10.10), right-aligned to a header ending at `centre_right`.
pub fn keys_header_rects(centre_right: f32) -> (Rect, Rect, Rect) {
    let next = Rect::new(
        centre_right - KEYS_HEADER_GAP - KEYS_NEXT_WIDTH,
        388.0,
        KEYS_NEXT_WIDTH,
        28.0,
    );
    let back = Rect::new(
        next.x - KEYS_BUTTON_GAP - KEYS_BACK_WIDTH,
        388.0,
        KEYS_BACK_WIDTH,
        28.0,
    );
    let field = Rect::new(
        back.x - KEYS_HEADER_GAP - KEYS_FIELD_WIDTH,
        388.0,
        KEYS_FIELD_WIDTH,
        28.0,
    );
    (field, back, next)
}

fn keys_header(paint: &mut Painter<'_, '_>, tab: &QueryTab, centre_right: f32) {
    let theme = paint.theme;
    let faces = paint.faces;
    let pages = &tab.key_pages;
    let (field, back, next) = keys_header_rects(centre_right);
    let error = pages.prefix_error();
    paint.button(field, Button::Secondary);
    if error.is_some() {
        paint.canvas.stroke_rect(field, theme.error);
    } else if pages.prefix_focused {
        paint.canvas.stroke_rect(field, theme.accent);
    }
    let icon = (field.x + 14.0, field.y + 12.0);
    paint
        .canvas
        .stroke_circle(icon.0, icon.1, 8.0, 1.5, theme.text_secondary);
    paint.canvas.line(
        (icon.0 + 3.0, icon.1 + 3.0),
        (icon.0 + 6.0, icon.1 + 6.0),
        1.5,
        theme.text_secondary,
    );
    let text_left = field.x + 28.0;
    let room = field.x + field.w - 8.0 - text_left;
    if pages.prefix_text.is_empty() {
        paint.text(
            faces.ui,
            "Prefix",
            text_left,
            393.0,
            BODY,
            theme.text_disabled,
        );
    } else {
        let shown = elided(faces.ui, &pages.prefix_text, room, BODY.0);
        let width = faces.ui.width(&shown, BODY.0);
        if pages.prefix_focused && pages.prefix_selected {
            paint.rect(text_left, field.y + 5.0, width, 18.0, theme.selection);
        }
        let ink = if tab.running {
            theme.text_disabled
        } else {
            theme.text_primary
        };
        paint.text(faces.ui, &shown, text_left, 393.0, BODY, ink);
    }
    if pages.prefix_focused && !pages.prefix_selected {
        let before: String = pages.prefix_text.chars().take(pages.prefix_caret).collect();
        let caret = text_left + faces.ui.width(&before, BODY.0).min(room);
        paint.rect(caret.round(), field.y + 5.0, 1.0, 18.0, theme.accent);
    }
    if let Some(line) = &error {
        paint.text(
            faces.ui,
            line,
            field.x,
            field.y + field.h + 2.0,
            SMALL,
            theme.error,
        );
    }
    for (rect, label, enabled, button) in [
        (back, "Back", pages.can_go_back() && !tab.running, PagingButton::Back),
        (next, "Next 100", pages.can_go_next() && !tab.running, PagingButton::Next),
    ] {
        paint.button(rect, Button::Ghost);
        if pages.paging_focus == Some(button) {
            focus_ring(paint, rect);
        }
        let ink = if enabled {
            theme.text_primary
        } else {
            theme.text_disabled
        };
        let width = faces.ui.width(label, BODY.0);
        paint.text(
            faces.ui,
            label,
            rect.x + (rect.w - width) / 2.0,
            393.0,
            BODY,
            ink,
        );
    }
}

fn keys_header_nodes(nodes: &mut Vec<AccessNode>, tab: &QueryTab, centre_right: f32) {
    let pages = &tab.key_pages;
    let (field, back, next) = keys_header_rects(centre_right);
    let mut prefix = node(
        NODE_KEY_PREFIX,
        NODE_MAIN_WINDOW,
        AccessRole::SearchBox,
        "Key prefix",
        (field.x, field.y, field.w, field.h),
    );
    prefix.value = Some(pages.prefix_text.clone());
    prefix.placeholder = Some("Prefix".to_owned());
    prefix.state.focused = pages.prefix_focused;
    prefix.state.disabled = tab.running;
    prefix.description = pages.prefix_error();
    prefix.state.invalid = prefix.description.is_some();
    prefix.actions.focus = true;
    prefix.actions.set_value = true;
    nodes.push(prefix);
    for (id, rect, name, shortcut, enabled, paging) in [
        (
            NODE_KEYS_BACK,
            back,
            "Back",
            "Alt+PgUp",
            pages.can_go_back(),
            PagingButton::Back,
        ),
        (
            NODE_KEYS_NEXT,
            next,
            "Next 100",
            "Alt+PgDn",
            pages.can_go_next(),
            PagingButton::Next,
        ),
    ] {
        let mut button = node(
            id,
            NODE_MAIN_WINDOW,
            AccessRole::Button,
            name,
            (rect.x, rect.y, rect.w, rect.h),
        );
        button.shortcut = Some(shortcut.to_owned());
        button.state.focused = pages.paging_focus == Some(paging);
        button.state.disabled = !enabled || tab.running;
        button.actions.invoke = enabled && !tab.running;
        nodes.push(button);
    }
}

const PLAN_SEGMENT_X: f32 = 428.0;
const PLAN_TOGGLE_WIDTH: f32 = 44.0;

fn result_segments(tab: &QueryTab, layout: Layout) -> Vec<(&'static str, ResultView, f32, f32)> {
    let [results, messages, plan] = layout.view_labels();
    if layout == Layout::EggDb {
        let mut segments = vec![
            (results, ResultView::Results, 272.0, 48.0),
            (messages, ResultView::Messages, 344.0, 64.0),
        ];
        if tab.plan.is_some() {
            segments.push((plan, ResultView::Plan, PLAN_SEGMENT_X, 32.0));
        }
        return segments;
    }
    let mut views = vec![
        (results, ResultView::Results),
        (messages, ResultView::Messages),
    ];
    if tab.plan.is_some() {
        views.push((plan, ResultView::Plan));
    }
    let mut x = 272.0;
    views
        .into_iter()
        .map(|(name, view)| {
            let underline = SEGMENT_ADVANCE * name.chars().count() as f32;
            let segment = (name, view, x, underline);
            x += underline + SEGMENT_GAP;
            segment
        })
        .collect()
}

const SEGMENT_ADVANCE: f32 = 7.5;
const SEGMENT_GAP: f32 = 24.0;

fn segment_width(layout: Layout, underline: f32) -> f32 {
    if layout == Layout::EggDb {
        72.0
    } else {
        underline + 16.0
    }
}

fn summary_x(tab: &QueryTab, layout: Layout) -> f32 {
    if layout != Layout::EggDb {
        return result_segments(tab, layout)
            .last()
            .map_or(PLAN_SEGMENT_X, |(_, _, x, underline)| {
                x + underline + SEGMENT_GAP
            });
    }
    if tab.plan.is_some() {
        PLAN_SEGMENT_X + 56.0
    } else {
        PLAN_SEGMENT_X
    }
}

fn plan_toggle_x(centre_right: f32) -> f32 {
    centre_right - 88.0 - PLAN_TOGGLE_WIDTH
}

fn plan_body(paint: &mut Painter<'_, '_>, plan: &PlanView, hovered: Option<usize>) {
    let theme = paint.theme;
    let faces = paint.faces;
    let (centre_width, centre_right) = (paint.centre_width(), paint.centre_right);
    let toggle = plan_toggle_x(centre_right);
    if plan.text_shown {
        paint.canvas.fill_rounded_rect(
            Rect::new(toggle, 390.0, PLAN_TOGGLE_WIDTH, 24.0),
            4.0,
            theme.bg2,
        );
    }
    paint.text(
        faces.ui,
        "Text",
        toggle + 8.0,
        393.0,
        BODY,
        theme.text_primary,
    );
    if plan.text_shown {
        for (index, line) in plan.plan.text.iter().enumerate().take(17) {
            let top = 426.0 + LINE_HEIGHT * index as f32;
            let shown = elided(faces.mono, line, centre_width - 20.0, CODE.0);
            paint.text(faces.mono, &shown, 272.0, top, CODE, theme.text_primary);
        }
        return;
    }
    plan_view::draw(paint, plan, plan_area(centre_width), hovered);
}

fn plan_area(centre_width: f32) -> Rect {
    Rect::new(260.0, 418.0, centre_width, BODY_BOTTOM - 418.0)
}

fn grid_handles(paint: &mut Painter<'_, '_>, grid: &ResultGrid, top: f32, window: &MainWindow) {
    let theme = paint.theme;
    if let Some(column) = window.column_edge {
        let edges = grid.column_edges();
        let edge = GRID_LEFT + edges.get(column + 1).copied().unwrap_or(0.0) - grid.scroll_x;
        let bottom = if window.resizing {
            BODY_BOTTOM
        } else {
            top + grid::HEADER_HEIGHT
        };
        if edge > GRID_LEFT {
            paint.rect(edge - 2.0, top, 2.0, bottom - top, theme.accent);
        }
    }
    for axis in [Axis::Vertical, Axis::Horizontal] {
        if let Some(track) = grid_track(grid, axis, paint.centre_right) {
            let extent = grid_extent(grid, axis, paint.centre_width());
            let hot = window.scrollbar_hot == Some(axis);
            scrollbar::draw(paint, axis, track, extent, hot);
        }
    }
}

fn grid_body(paint: &mut Painter<'_, '_>, grid: &ResultGrid, top: f32) {
    let theme = paint.theme;
    let faces = paint.faces;
    let centre_width = paint.centre_width();
    paint.rect(260.0, top, centre_width, 28.0, theme.bg1);
    paint.rect(260.0, top + 27.0, centre_width, 1.0, theme.border);
    let edges = grid.column_edges();
    let columns = visible_columns(&grid.widths, grid.scroll_x, centre_width - 48.0);
    let x_of =
        |column: usize| GRID_LEFT + edges.get(column).copied().unwrap_or(0.0) - grid.scroll_x;
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
            paint.text_right(
                faces.ui,
                &name,
                right - 12.0,
                top + 5.0,
                BODY,
                theme.text_primary,
            );
        } else {
            paint.text(
                faces.ui,
                &name,
                left + 12.0,
                top + 5.0,
                BODY,
                theme.text_primary,
            );
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
                let color = if grid.focused {
                    theme.selection
                } else {
                    theme.selection_inactive
                };
                paint.rect(left, y, right - left, 24.0, color);
            }
            let kind = grid.kind(column);
            let (text, look) = grid.cell_look(cell);
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
    let left = status_bar_left(&window.tab);
    paint.text(faces.ui, &left, 260.0, 781.0, SMALL, theme.text_secondary);
    if let Some((text, color)) = transaction_status_item(window, theme) {
        let x = if left.is_empty() {
            260.0
        } else {
            260.0 + faces.ui.width(&left, SMALL.0) + 16.0
        };
        paint.text(faces.ui, &text, x, 781.0, SMALL, color);
    }
    if let Some(item) = window.layout.status_item() {
        let shown = elided(faces.ui, item, LAYOUT_ITEM_WIDTH, SMALL.0);
        paint.text_right(
            faces.ui,
            &shown,
            paint.width - LAYOUT_ITEM_RIGHT,
            781.0,
            SMALL,
            theme.accent,
        );
    }
    let caret = window.tab.editor.caret();
    let mut right = paint.width - 12.0;
    let mut items = vec![window.tab.pill_label().to_owned()];
    if !window.tab.is_keys() {
        items.push(format!("Ln {}, Col {}", caret.line + 1, caret.column + 1));
    }
    items.push("UTF-8".to_owned());
    for item in items {
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

/// The status bar's transaction item (`docs/ux/workspace.md` 9.2) and its colour: the
/// showing tab's open or failed transaction, else another tab's open one.
fn transaction_status_item(window: &MainWindow, theme: Theme) -> Option<(String, Rgba)> {
    if let Some((text, failed)) = window.tab.transaction_item() {
        let color = if failed { theme.error } else { theme.warning };
        return Some((text.to_owned(), color));
    }
    let other = window.other_tab_in_transaction()?;
    Some((transaction_open_in(&other.title), theme.text_secondary))
}

/// The status bar's item while another tab than the showing one holds a transaction.
pub fn transaction_open_in(title: &str) -> String {
    format!("Transaction open in {title}")
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
    let menu = Rect::new(
        paint.width - 224.0,
        40.0,
        160.0,
        28.0 * QueryDialect::ALL.len() as f32,
    );
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
                &[
                    (menu.x + 10.0, top + 14.0),
                    (menu.x + 14.0, top + 18.0),
                    (menu.x + 22.0, top + 10.0),
                ],
                1.5,
                theme.accent,
            );
        }
        paint.text(
            faces.ui,
            dialect.label(),
            menu.x + 32.0,
            top + 5.0,
            BODY,
            theme.text_primary,
        );
    }
}

fn grid_top(grid: &ResultGrid) -> f32 {
    if grid.truncated() { 446.0 } else { 418.0 }
}

/// The rect of `cell` of `grid` in the EggDB layout's coordinates, scrolled as the grid is.
pub fn grid_cell_rect(grid: &ResultGrid, cell: Cell) -> Rect {
    let edges = grid.column_edges();
    let x_of =
        |column: usize| GRID_LEFT + edges.get(column).copied().unwrap_or(0.0) - grid.scroll_x;
    let rows_above = cell.row as f32 - grid.first_row as f32;
    let top = grid_top(grid) + grid::HEADER_HEIGHT + grid::ROW_HEIGHT * rows_above;
    let left = x_of(cell.column);
    Rect::new(left, top, x_of(cell.column + 1) - left, grid::ROW_HEIGHT)
}

fn shifted(rect: Rect, shift: (f32, f32)) -> Rect {
    Rect::new(rect.x + shift.0, rect.y + shift.1, rect.w, rect.h)
}

fn rect_contains(rect: Rect, x: f32, y: f32) -> bool {
    x >= rect.x && y >= rect.y && x < rect.x + rect.w && y < rect.y + rect.h
}

fn is_value_view_node(id: u32) -> bool {
    (value_view::NODE_VALUE_VIEW..=value_view::NODE_VALUE_COPY).contains(&id)
}

fn grid_menu_item_of(id: u32) -> Option<usize> {
    let index = id.checked_sub(grid_menu::NODE_FIRST_GRID_MENU_ITEM)? as usize;
    (index < grid::GridMenuItem::ALL.len()).then_some(index)
}

impl MainWindow {
    fn results_shift(&self) -> (f32, f32) {
        self.placed(Area::Results)
            .map_or((0.0, 0.0), |placed| placed.shift)
    }

    /// Where the open full-value view shows, in window coordinates.
    pub fn value_view_rect(&self) -> Option<Rect> {
        let view = self.tab.value_view.as_ref()?;
        let grid = self.tab.grid.as_ref()?;
        let cell = grid_cell_rect(grid, view.cell);
        let rect = value_view::popover_rect(cell, self.width as f32);
        Some(shifted(rect, self.results_shift()))
    }

    /// Where the grid's open menu shows, in window coordinates.
    pub fn grid_menu_rect(&self) -> Option<Rect> {
        let menu = self.tab.grid_menu.as_ref()?;
        let grid = self.tab.grid.as_ref()?;
        let width = self.width as f32;
        let cell = shifted(grid_cell_rect(grid, grid.current), self.results_shift());
        Some(grid_menu::rect_for(menu.origin, cell, width))
    }

    /// Where the editor's open menu shows, in window coordinates.
    pub fn editor_menu_rect(&self) -> Option<Rect> {
        let menu = self.tab.editor_menu.as_ref()?;
        Some(editor_menu::rect_for(menu.origin, self.width as f32))
    }

    /// The open popover's rect: the value view's, the grid menu's, the editor menu's or the
    /// tree's row menu's.
    pub fn popover_rect(&self) -> Option<Rect> {
        self.value_view_rect()
            .or_else(|| self.grid_menu_rect())
            .or_else(|| self.editor_menu_rect())
            .or_else(|| self.tree_menu_rect())
    }

    /// Closes the value view, the grid's and the editor's menus and the tree's row menu.
    pub fn close_popovers(&mut self) {
        self.tab.value_view = None;
        self.tab.grid_menu = None;
        self.tab.editor_menu = None;
        self.tree_menu = None;
    }

    /// Whether node `id` belongs to the open popover.
    pub fn is_popover_node(&self, id: u32) -> bool {
        (self.tab.value_view.is_some() && is_value_view_node(id))
            || (self.tab.grid_menu.is_some()
                && (id == grid_menu::NODE_GRID_MENU || grid_menu_item_of(id).is_some()))
            || (self.tree_menu.is_some()
                && (id == tree_menu::NODE_TREE_MENU || tree_menu::item_of_node(id).is_some()))
            || (self.tab.editor_menu.is_some()
                && (id == editor_menu::NODE_EDITOR_MENU || editor_menu::item_of_node(id).is_some()))
    }

    /// A press of the primary button while a popover is open: outside it closes the popover
    /// and the press does nothing else (`docs/ux/workspace.md` 9.3, 9.4). Returns whether a
    /// popover was open and the press was outside it.
    pub fn press_outside_popover(&mut self, x: f32, y: f32) -> bool {
        match self.popover_rect() {
            Some(rect) if !rect_contains(rect, x, y) => {
                self.close_popovers();
                true
            }
            _ => false,
        }
    }

    /// The grid's menu item under (`x`, `y`), while the menu is open.
    pub fn grid_menu_item_at(&self, x: f32, y: f32) -> Option<usize> {
        grid_menu::item_at(self.grid_menu_rect()?, x, y)
    }

    /// A right-click at (`x`, `y`) over the results (`docs/ux/workspace.md` 9.4): on a cell
    /// outside the selection it makes that cell current, inside it keeps the selection; on
    /// the gutter or a header it selects that row or column; then the grid's menu opens at
    /// the pointer. Returns whether it opened.
    pub fn context_press(&mut self, tree: &[AccessNode], x: f32, y: f32) -> bool {
        self.close_popovers();
        if let Some(opened) = self.tree_context_press(tree, x, y) {
            return opened;
        }
        if self.tab.view != ResultView::Results {
            return false;
        }
        let hit = |role: AccessRole| {
            tree.iter()
                .rev()
                .find(|node| node.role == role && pointer::contains(node.bounds, x, y))
                .and_then(|node| node.cell)
        };
        let (cell, header) = (hit(AccessRole::Cell), hit(AccessRole::ColumnHeader));
        let shift = self.results_shift();
        let Some(grid) = self.tab.grid.as_mut().filter(|grid| !grid.rows.is_empty()) else {
            return false;
        };
        let body_top = grid_top(grid) + grid::HEADER_HEIGHT + shift.1;
        let gutter = (SIDEBAR_RIGHT + shift.0..GRID_LEFT - 1.0 + shift.0).contains(&x)
            && (body_top..BODY_BOTTOM + shift.1).contains(&y);
        if let Some(at) = cell {
            let cell = Cell {
                row: at.row,
                column: at.column,
            };
            if !grid.is_selected(cell) {
                grid.current = cell;
                grid.anchor = cell;
            }
        } else if let Some(at) = header {
            grid.current.column = at.column.min(grid.columns.len().saturating_sub(1));
            grid.menu_item(grid::GridMenuItem::SelectColumn);
        } else if gutter {
            let row = grid.first_row + ((y - body_top) / grid::ROW_HEIGHT) as usize;
            if row >= grid.rows.len() {
                return false;
            }
            grid.current.row = row;
            grid.menu_item(grid::GridMenuItem::SelectRow);
        } else {
            return false;
        }
        self.focus = WindowFocus::Tab;
        self.tab.focus(TabFocus::Grid);
        self.tab.open_grid_menu(GridMenu::at_pointer(x, y));
        true
    }

    fn popover_access_keys(
        &mut self,
        node: &AccessNode,
        action: &AccessAction,
    ) -> Option<Vec<KeyPress>> {
        if self.tab.value_view.is_some() {
            if !is_value_view_node(node.id) {
                self.tab.value_view = None;
                return None;
            }
            let copy = node.id == value_view::NODE_VALUE_COPY;
            if let Some(view) = &mut self.tab.value_view {
                view.copy_focused = copy;
            }
            let invoked = copy && *action == AccessAction::Invoke;
            return Some(if invoked {
                vec![KeyPress::plain(Key::Enter)]
            } else {
                Vec::new()
            });
        }
        if self.tree_menu.is_some() {
            let Some(index) = tree_menu::item_of_node(node.id) else {
                self.tree_menu = None;
                return None;
            };
            if let Some(menu) = &mut self.tree_menu {
                menu.highlighted = Some(index);
            }
            let invoked = *action == AccessAction::Invoke;
            return Some(if invoked {
                vec![KeyPress::plain(Key::Enter)]
            } else {
                Vec::new()
            });
        }
        if self.tab.editor_menu.is_some() {
            let Some(index) = editor_menu::item_of_node(node.id) else {
                self.tab.editor_menu = None;
                return None;
            };
            if let Some(menu) = &mut self.tab.editor_menu {
                menu.highlighted = Some(index);
            }
            let invoked = *action == AccessAction::Invoke;
            return Some(if invoked {
                vec![KeyPress::plain(Key::Enter)]
            } else {
                Vec::new()
            });
        }
        if self.tab.grid_menu.is_some() {
            let Some(index) = grid_menu_item_of(node.id) else {
                self.tab.grid_menu = None;
                return None;
            };
            if let Some(menu) = &mut self.tab.grid_menu {
                menu.highlighted = Some(index);
            }
            let invoked = *action == AccessAction::Invoke;
            return Some(if invoked {
                vec![KeyPress::plain(Key::Enter)]
            } else {
                Vec::new()
            });
        }
        None
    }
}

impl MainWindow {
    fn sidebar_shift(&self) -> (f32, f32) {
        self.placed(Area::Sidebar)
            .map_or((0.0, 0.0), |placed| placed.shift)
    }

    /// Where the tree's open row menu shows, in window coordinates.
    pub fn tree_menu_rect(&self) -> Option<Rect> {
        let menu = self.tree_menu.as_ref()?;
        let rows = self.tree.rows();
        let at = rows
            .iter()
            .position(|row| row.item == menu.row)
            .unwrap_or(0);
        let level = rows.get(at).map_or(0, |row| row.level);
        let shift = self.sidebar_shift();
        let label = (
            44.0 + 16.0 * level as f32 + shift.0,
            ROWS_TOP + ROW_HEIGHT * at.saturating_sub(self.tree.first_row) as f32 + shift.1,
        );
        Some(tree_menu::rect_for(menu.origin, label, self.width as f32))
    }

    /// The tree's menu item under (`x`, `y`), while the menu is open.
    pub fn tree_menu_item_at(&self, x: f32, y: f32) -> Option<usize> {
        tree_menu::item_at(self.tree_menu_rect()?, x, y)
    }

    fn tree_context_press(&mut self, tree: &[AccessNode], x: f32, y: f32) -> Option<bool> {
        let row = tree
            .iter()
            .rev()
            .find(|node| node.role == AccessRole::TreeItem && pointer::contains(node.bounds, x, y))
            .cloned();
        if let Some(row) = row {
            if !self.focus_node(&row) {
                return Some(false);
            }
            let item = self.tree.current;
            self.tree.selected = Some(item);
            self.open_tree_menu(item, TreeMenuOrigin::Pointer { x, y });
            return Some(true);
        }
        let shift = self.sidebar_shift();
        let in_sidebar = self.placed(Area::Sidebar).is_some()
            && area_at(x - shift.0, y - shift.1) == Area::Sidebar;
        if in_sidebar {
            self.focus_tree();
            return Some(false);
        }
        None
    }

    /// Opens the row menu on `row` from `origin` (`docs/ux/workspace.md` 10.1), enabling
    /// the items its kind of row has, `Drop…` only on a writable connection.
    pub fn open_tree_menu(&mut self, row: TreeItem, origin: TreeMenuOrigin) {
        self.close_popovers();
        let expanded = self
            .tree
            .rows()
            .into_iter()
            .find(|shown| shown.item == row)
            .and_then(|shown| shown.expanded);
        let copyable = self.copied_name(row).is_some();
        let writable = self.writable;
        let enabled = TreeMenuItem::ALL.map(|item| item.enabled(row, copyable, writable));
        self.focus_tree();
        self.tree.current = row;
        self.tree_menu = Some(TreeMenu::new(row, expanded, enabled, origin));
    }

    fn tree_menu_answer(&mut self, answer: TreeMenuAnswer, now: Instant) -> WindowAction {
        let none = WindowAction::Tab(TabAction::None);
        match answer {
            TreeMenuAnswer::Moved => none,
            TreeMenuAnswer::Close => {
                self.tree_menu = None;
                self.focus_tree();
                none
            }
            TreeMenuAnswer::Activate(item) => match self.tree_menu.take() {
                Some(menu) => self.tree_menu_item(item, menu.row, now),
                None => none,
            },
        }
    }

    fn tree_menu_item(&mut self, item: TreeMenuItem, row: TreeItem, now: Instant) -> WindowAction {
        self.focus_tree();
        self.tree.current = row;
        match item {
            TreeMenuItem::Open => {
                let action = self.tree.key(KeyPress::plain(Key::Enter), now);
                self.tree_action(action, WindowFocus::Tree)
            }
            TreeMenuItem::InsertName => {
                self.tree_action(TreeAction::InsertName(row), WindowFocus::Tree)
            }
            TreeMenuItem::CopyName => self.copy_name(row),
            TreeMenuItem::ShowDdl => self.show_ddl(row),
            TreeMenuItem::Refresh => WindowAction::RefreshCatalog,
            TreeMenuItem::Drop => {
                self.open_drop_prompt(row);
                WindowAction::Tab(TabAction::None)
            }
        }
    }

    /// What `Copy name` copies for `row`: the connection's path or URL, else the row's name.
    pub fn copied_name(&self, row: TreeItem) -> Option<String> {
        match row {
            TreeItem::Connection => Some(self.location.clone()),
            other => self.tree.name_of(other),
        }
    }

    /// `Ctrl/Cmd+C` in the tree and `Copy name`: puts the name on the clipboard with the
    /// toast `Copied orders`.
    pub fn copy_name(&mut self, row: TreeItem) -> WindowAction {
        match self.copied_name(row) {
            Some(text) => {
                let toast = format!("Copied {text}");
                WindowAction::Tab(TabAction::Copy { text, toast })
            }
            None => WindowAction::Tab(TabAction::None),
        }
    }

    /// `Show DDL`: selects the row (a column's table), opens the inspector and unfolds its
    /// `DDL` block.
    pub fn show_ddl(&mut self, row: TreeItem) -> WindowAction {
        let item = match row {
            TreeItem::Column { table, .. } => TreeItem::Table(table),
            other => other,
        };
        self.tree.selected = Some(item);
        if !self.inspector.is_open() {
            self.inspector.toggle(self.width);
        }
        self.inspector
            .collapsed
            .retain(|&block| block != InspectorBlock::Ddl);
        self.inspector.ddl_focused = true;
        WindowAction::Inspect(item)
    }

    fn keys_header_access(&mut self, id: u32, action: &AccessAction) -> Option<Vec<KeyPress>> {
        if !self.tab.is_keys() {
            return None;
        }
        let alt = |key: Key| KeyPress {
            key,
            modifiers: eggdb_platform::window::Modifiers {
                alt: true,
                ..Default::default()
            },
        };
        match id {
            NODE_KEY_PREFIX => {
                self.focus_tab();
                self.tab.focus_prefix();
                if let AccessAction::SetValue(text) = action {
                    self.tab.key_pages.prefix_text = text.clone();
                    self.tab.key_pages.prefix_caret_to_end();
                    return Some(vec![KeyPress::plain(Key::Enter)]);
                }
                Some(Vec::new())
            }
            NODE_KEYS_BACK | NODE_KEYS_NEXT if *action == AccessAction::Invoke => {
                self.focus_tab();
                let key = if id == NODE_KEYS_BACK {
                    Key::PageUp
                } else {
                    Key::PageDown
                };
                Some(vec![alt(key)])
            }
            NODE_KEYS_BACK | NODE_KEYS_NEXT => Some(Vec::new()),
            _ => None,
        }
    }

    fn ddl_key(&mut self, press: KeyPress) -> WindowAction {
        let primary = press.modifiers.control || press.modifiers.command;
        match press.key {
            Key::Character('c' | 'C') if primary => {
                let description = inspector::describe(
                    &self.tree,
                    self.inspected(),
                    self.inspector.sample.as_ref(),
                );
                let Some(text) = description.ddl else {
                    return WindowAction::Tab(TabAction::None);
                };
                let toast = crate::query_tab::copied_characters_toast(text.chars().count());
                self.tab.notice = Some(toast.clone());
                WindowAction::Tab(TabAction::Copy { text, toast })
            }
            Key::Escape | Key::Tab => {
                self.inspector.ddl_focused = false;
                self.focus_tree();
                WindowAction::Tab(TabAction::None)
            }
            _ => WindowAction::Tab(TabAction::None),
        }
    }

    /// Opens the editor's right-click menu from `Shift+F10` or the menu key
    /// (`docs/ux/workspace.md` 10.9): below the caret's line box (the selection's end), or in
    /// a keys tab below the line's start at x 320.
    pub fn open_editor_menu_at_caret(&mut self) {
        let shift = self
            .placed(Area::Editor)
            .map_or((0.0, 0.0), |placed| placed.shift);
        let tab = &self.tab;
        let lines = tab.editor.lines();
        let at = if tab.is_keys() {
            crate::editor::Position::default()
        } else {
            tab.editor
                .selection()
                .map_or(tab.editor.caret(), |(_, end)| end)
        };
        let visible =
            (((editor_bottom(tab) - EDITOR_TEXT_TOP) / LINE_HEIGHT).floor() as usize).max(1);
        let first =
            first_visible_line(tab.editor.caret().line, visible).min(lines.len().saturating_sub(1));
        let row = at.line.clamp(first, first + visible - 1) - first;
        let before: String = lines
            .get(at.line)
            .map(|line| line.chars().take(at.column).collect())
            .unwrap_or_default();
        let origin = EditorMenuOrigin::Caret {
            x: EDITOR_TEXT_LEFT + crate::grid::measured(&before) + shift.0,
            line_top: EDITOR_TEXT_TOP + LINE_HEIGHT * row as f32 + shift.1,
            line_height: LINE_HEIGHT,
        };
        let clipboard = self.clipboard_has_text;
        self.tab.open_editor_menu(origin, clipboard);
    }

    /// A right-click at (`x`, `y`) over the editor (`docs/ux/workspace.md` 10.9): on the text
    /// the caret moves to the click (a click inside the selection keeps it), on the gutter
    /// the line is selected, then the editor's menu opens at the pointer; the error strip and
    /// the scrollbar open nothing. Returns whether it opened.
    pub fn editor_context_press(
        &mut self,
        faces: Faces<'_>,
        tree: &[AccessNode],
        x: f32,
        y: f32,
    ) -> bool {
        let Some(editor) = tree
            .iter()
            .find(|node| node.id == NODE_EDITOR && pointer::contains(node.bounds, x, y))
        else {
            return false;
        };
        let shift = self
            .placed(Area::Editor)
            .map_or((0.0, 0.0), |placed| placed.shift);
        let scrollbar = x >= editor.bounds.x + editor.bounds.width - 12.0;
        let strip = y - shift.1 >= editor_bottom(&self.tab);
        if scrollbar || strip {
            return false;
        }
        self.close_popovers();
        self.focus_tab();
        if !self.tab.is_keys() {
            let at = self.editor_position_at(faces, x, y);
            if x - shift.0 < EDITOR_GUTTER_RIGHT {
                self.tab.editor.select_line(at.line);
            } else {
                let inside = self
                    .tab
                    .editor
                    .selection()
                    .is_some_and(|(start, end)| start <= at && at <= end);
                if !inside {
                    self.tab.editor.place_caret(at);
                }
            }
        }
        let clipboard = self.clipboard_has_text;
        self.tab
            .open_editor_menu(EditorMenuOrigin::Pointer { x, y }, clipboard);
        true
    }

    fn drop_target(&self, row: TreeItem) -> Option<DropTarget> {
        let catalog = &self.tree.catalog;
        let kind = match row {
            TreeItem::Table(_) => DropKind::Table,
            TreeItem::View(_) => DropKind::View,
            TreeItem::Index(index) | TreeItem::TableIndex(index) => DropKind::Index {
                table: catalog.indexes.get(index)?.table.clone(),
            },
            TreeItem::Sequence(_) => DropKind::Sequence,
            TreeItem::Trigger(trigger) => DropKind::Trigger {
                table: catalog.triggers.get(trigger)?.table.clone(),
            },
            _ => return None,
        };
        Some(DropTarget {
            kind,
            name: self.tree.name_of(row)?,
        })
    }

    fn selected_after_drop(&self, row: TreeItem) -> TreeItem {
        use crate::object_browser::Group;
        match row {
            TreeItem::Table(_) => TreeItem::Group(Group::Tables),
            TreeItem::View(_) => TreeItem::Group(Group::Views),
            TreeItem::Index(_) => TreeItem::Group(Group::Indexes),
            TreeItem::Sequence(_) => TreeItem::Group(Group::Sequences),
            other => self.tree.table_of(other).unwrap_or(TreeItem::Connection),
        }
    }

    /// Why a drop cannot run now: a tab runs, or a tab's session holds a transaction.
    pub fn drop_blocked(&self) -> Option<String> {
        let tabs = || std::iter::once(&self.tab).chain(self.background_tabs.iter());
        if let Some(tab) = tabs().find(|tab| tab.running) {
            return Some(drop_prompt::blocked_body(&tab.title, true));
        }
        tabs()
            .find(|tab| tab.transaction != eggdb::sql::TransactionStatus::Idle)
            .map(|tab| drop_prompt::blocked_body(&tab.title, false))
    }

    /// Re-reads why the open Drop dialog cannot drop, after a tab's run ended or its
    /// transaction changed, so `Drop` acts as soon as nothing blocks it.
    pub fn refresh_drop_blocked(&mut self) {
        let blocked = self.drop_blocked();
        if let Some((prompt, _)) = &mut self.drop_prompt {
            prompt.blocked = blocked;
        }
    }

    /// `Drop…` on `row`: opens the dialog with the statement in the showing tab's dialect;
    /// nothing on a read-only connection or a row that cannot be dropped.
    pub fn open_drop_prompt(&mut self, row: TreeItem) {
        if !self.writable {
            return;
        }
        let Some(target) = self.drop_target(row) else {
            return;
        };
        let prompt = DropPrompt::new(target, self.tab.dialect, self.drop_blocked());
        self.drop_prompt = Some((prompt, self.selected_after_drop(row)));
    }

    fn drop_answer(&mut self, answer: DropAnswer) -> WindowAction {
        match answer {
            DropAnswer::Pending => WindowAction::Tab(TabAction::None),
            DropAnswer::Cancel => {
                self.drop_prompt = None;
                self.focus_tree();
                WindowAction::Tab(TabAction::None)
            }
            DropAnswer::Drop(statement) => WindowAction::Drop {
                statement,
                dialect: self.tab.dialect,
            },
        }
    }

    /// The drop ran: the dialog closes, the dropped row's parent is selected and the toast
    /// says what went; returns the toast.
    pub fn drop_succeeded(&mut self) -> Option<String> {
        let (prompt, then_selected) = self.drop_prompt.take()?;
        self.tree.selected = Some(then_selected);
        self.tree.current = then_selected;
        self.focus_tree();
        let toast = drop_prompt::dropped_toast(&prompt.target);
        self.tab.notice = Some(toast.clone());
        Some(toast)
    }

    /// The drop failed: the dialog stays with the engine's error line and the full report
    /// goes to the showing tab's messages.
    pub fn drop_failed(&mut self, failure: &crate::connection::StatementFailure) {
        if let Some((prompt, _)) = &mut self.drop_prompt {
            prompt.failed(drop_prompt::error_line(&failure.code, &failure.message));
        }
        self.tab.messages.extend(failure.report.iter().cloned());
    }
}

/// What a press on the grid's column edges or scrollbars started, for the drag that follows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GridHandle {
    /// A column's right edge, grabbed `grab` pixels right of it.
    ColumnEdge {
        /// The column.
        column: usize,
        /// Where the pointer is from the edge.
        grab: f32,
    },
    /// A scrollbar's thumb, grabbed `grab` pixels from its start.
    Thumb {
        /// Which scrollbar.
        axis: Axis,
        /// Where the pointer is from the thumb's start.
        grab: f32,
    },
    /// A splitter between regions (`docs/ux/workspace.md` 10.5).
    Splitter(Splitter),
    /// A double-click auto-fitted a column or reset a splitter, a press on a track paged, or
    /// a press opened or closed the drawer; nothing to drag.
    Done,
}

const EDGE_BAND_LEFT: f32 = 3.0;
const EDGE_BAND_RIGHT: f32 = 2.0;

fn grid_rows_in_view(grid: &ResultGrid) -> usize {
    let body_top = grid_top(grid) + grid::HEADER_HEIGHT;
    ((BODY_BOTTOM - body_top) / grid::ROW_HEIGHT)
        .floor()
        .max(1.0) as usize
}

/// The grid's extent along `axis` in a centre `centre_width` wide (`docs/ux/workspace.md`
/// 9.6): whole rows down, pixels across past the gutter.
pub fn grid_extent(grid: &ResultGrid, axis: Axis, centre_width: f32) -> Extent {
    match axis {
        Axis::Vertical => Extent {
            viewport: grid_rows_in_view(grid) as f32 * grid::ROW_HEIGHT,
            content: grid.rows.len() as f32 * grid::ROW_HEIGHT,
            offset: grid.first_row as f32 * grid::ROW_HEIGHT,
        },
        Axis::Horizontal => Extent {
            viewport: centre_width - grid::GUTTER_WIDTH,
            content: grid.content_width(),
            offset: grid.scroll_x,
        },
    }
}

/// The grid's scrollbar track along `axis` in the EggDB layout's coordinates, `None` while
/// its content fits: at the body's right or bottom edge, each 10 shorter while the other
/// shows.
pub fn grid_track(grid: &ResultGrid, axis: Axis, centre_right: f32) -> Option<Rect> {
    let centre_width = centre_right - SIDEBAR_RIGHT;
    let vertical = grid_extent(grid, Axis::Vertical, centre_width).scrolls();
    let horizontal = grid_extent(grid, Axis::Horizontal, centre_width).scrolls();
    let body_top = grid_top(grid) + grid::HEADER_HEIGHT;
    match axis {
        Axis::Vertical if vertical => {
            let end = if horizontal {
                BODY_BOTTOM - scrollbar::TRACK
            } else {
                BODY_BOTTOM
            };
            Some(scrollbar::track_rect(
                axis,
                body_top,
                end - body_top,
                centre_right,
                BODY_BOTTOM,
            ))
        }
        Axis::Horizontal if horizontal => {
            let end = if vertical {
                centre_right - scrollbar::TRACK
            } else {
                centre_right
            };
            Some(scrollbar::track_rect(
                axis,
                GRID_LEFT,
                end - GRID_LEFT,
                centre_right,
                BODY_BOTTOM,
            ))
        }
        _ => None,
    }
}

fn along(axis: Axis, track: Rect, x: f32, y: f32) -> (f32, f32) {
    match axis {
        Axis::Vertical => (y - track.y, track.h),
        Axis::Horizontal => (x - track.x, track.w),
    }
}

impl MainWindow {
    fn results_grid(&self) -> Option<&ResultGrid> {
        self.tab
            .grid
            .as_ref()
            .filter(|_| self.tab.view == ResultView::Results)
    }

    fn edge_x(&self, grid: &ResultGrid, column: usize) -> f32 {
        let edges = grid.column_edges();
        GRID_LEFT + edges.get(column).copied().unwrap_or(0.0) - grid.scroll_x
            + self.results_shift().0
    }

    /// The column whose right edge's 6-pixel band in the header holds (`x`, `y`); the
    /// gutter's edge is not one.
    pub fn column_edge_at(&self, x: f32, y: f32) -> Option<usize> {
        let grid = self.results_grid()?;
        let shift = self.results_shift();
        let top = grid_top(grid) + shift.1;
        if y < top || y >= top + grid::HEADER_HEIGHT {
            return None;
        }
        let centre_width = self.centre_right() - SIDEBAR_RIGHT;
        visible_columns(
            &grid.widths,
            grid.scroll_x,
            centre_width - grid::GUTTER_WIDTH,
        )
        .find(|&column| {
            let edge = self.edge_x(grid, column + 1);
            edge > GRID_LEFT + shift.0 && x >= edge - EDGE_BAND_LEFT && x <= edge + EDGE_BAND_RIGHT
        })
    }

    fn grid_track_at(&self, x: f32, y: f32) -> Option<(Axis, Rect)> {
        let grid = self.results_grid()?;
        let shift = self.results_shift();
        [Axis::Vertical, Axis::Horizontal]
            .into_iter()
            .filter_map(|axis| {
                grid_track(grid, axis, self.centre_right())
                    .map(|track| (axis, shifted(track, shift)))
            })
            .find(|(_, track)| rect_contains(*track, x, y))
    }

    fn set_grid_offset(&mut self, axis: Axis, offset: f32) -> bool {
        let Some(grid) = self.tab.grid.as_mut() else {
            return false;
        };
        let before = (grid.first_row, grid.scroll_x);
        match axis {
            Axis::Vertical => {
                let last = grid.rows.len().saturating_sub(grid_rows_in_view(grid));
                grid.first_row = ((offset / grid::ROW_HEIGHT).round() as usize).min(last);
            }
            Axis::Horizontal => grid.scroll_x = offset.max(0.0),
        }
        (grid.first_row, grid.scroll_x) != before
    }

    /// The pointer moved to (`x`, `y`) with no drag: the column edge and the scrollbar under
    /// it light up. Returns whether that changed.
    pub fn hover_grid_handles(&mut self, x: f32, y: f32) -> bool {
        if self.resizing || self.dragged_splitter.is_some() {
            return false;
        }
        let before = (self.column_edge, self.scrollbar_hot, self.hot_splitter);
        self.column_edge = self.column_edge_at(x, y);
        self.scrollbar_hot = self.grid_track_at(x, y).map(|(axis, _)| axis);
        self.hot_splitter = if self.drawer_open {
            None
        } else {
            self.splitter_at(x, y)
        };
        (self.column_edge, self.scrollbar_hot, self.hot_splitter) != before
    }

    /// A press at (`x`, `y`) on the window's own handles (`docs/ux/workspace.md` 10.5, 10.6):
    /// outside the open drawer it closes it; on a rail item it opens the drawer; on a
    /// splitter it starts a drag, or resets the size on a double-click. `None` when the press
    /// is on none of them.
    pub fn press_layout_handle(&mut self, x: f32, y: f32, clicks: u8) -> Option<GridHandle> {
        if self.layout != Layout::EggDb || y < TOOLBAR_HEIGHT || y >= BODY_BOTTOM {
            return None;
        }
        if self.drawer_open {
            if x < splitters::SIDEBAR_DEFAULT {
                return None;
            }
            self.close_drawer();
            return Some(GridHandle::Done);
        }
        if self.rail_shown() && x < RAIL_WIDTH {
            if let Some(item) = self.rail_item_at(y) {
                self.open_drawer(item);
            }
            return Some(GridHandle::Done);
        }
        let splitter = self.splitter_at(x, y)?;
        self.focused_splitter = None;
        if clicks == 2 {
            self.sizes_changed |= self
                .sizes
                .set_size(splitter, RegionSizes::default().size(splitter));
            self.tab.notice = Some(self.sizes.notice(splitter));
            return Some(GridHandle::Done);
        }
        self.dragged_splitter = Some(splitter);
        self.hot_splitter = Some(splitter);
        self.tab.notice = Some(self.sizes.notice(splitter));
        Some(GridHandle::Splitter(splitter))
    }

    fn drag_splitter(&mut self, splitter: Splitter, x: f32, y: f32) -> bool {
        let size = match splitter {
            Splitter::Sidebar => x + 1.0,
            Splitter::Results => BODY_BOTTOM - y - splitters::BAND / 2.0,
            Splitter::Inspector => self.width as f32 - x,
        };
        let changed = self.sizes.set_size(splitter, size);
        self.tab.notice = Some(self.sizes.notice(splitter));
        changed
    }

    /// A press at (`x`, `y`), `clicks` of a multi-click (`docs/ux/workspace.md` 9.5, 9.6): on
    /// a column edge it starts a resize, or auto-fits on a double-click; on a scrollbar's
    /// thumb it starts a drag, and on its track it pages toward the pointer. `None` when the
    /// press is on neither.
    pub fn press_grid_handle(&mut self, x: f32, y: f32, clicks: u8) -> Option<GridHandle> {
        if let Some(column) = self.column_edge_at(x, y) {
            let grid = self.tab.grid.as_ref()?;
            let edge = self.edge_x(grid, column + 1);
            let grid = self.tab.grid.as_mut()?;
            if clicks == 2 {
                grid.auto_fit(column);
                self.tab.notice = Some(grid.width_text(column));
                return Some(GridHandle::Done);
            }
            self.tab.notice = Some(grid.width_text(column));
            self.column_edge = Some(column);
            self.resizing = true;
            return Some(GridHandle::ColumnEdge {
                column,
                grab: x - edge,
            });
        }
        let (axis, track) = self.grid_track_at(x, y)?;
        let grid = self.tab.grid.as_ref()?;
        let extent = grid_extent(grid, axis, self.centre_right() - SIDEBAR_RIGHT);
        let (pointer, length) = along(axis, track, x, y);
        let thumb = scrollbar::thumb(length, extent)?;
        if pointer >= thumb.start && pointer < thumb.start + thumb.length {
            self.scrollbar_hot = Some(axis);
            return Some(GridHandle::Thumb {
                axis,
                grab: pointer - thumb.start,
            });
        }
        let offset = scrollbar::paged_offset(pointer, length, extent);
        self.set_grid_offset(axis, offset);
        Some(GridHandle::Done)
    }

    /// The pointer dragged `handle` to (`x`, `y`): the column takes the width up to the
    /// pointer, or the thumb follows it. Returns whether the frame changed.
    pub fn drag_grid_handle(&mut self, handle: GridHandle, x: f32, y: f32) -> bool {
        match handle {
            GridHandle::Splitter(splitter) => self.drag_splitter(splitter, x, y),
            GridHandle::ColumnEdge { column, grab } => {
                let Some(grid) = self.tab.grid.as_ref() else {
                    return false;
                };
                let left = self.edge_x(grid, column);
                let Some(grid) = self.tab.grid.as_mut() else {
                    return false;
                };
                let changed = grid.set_width(column, x - grab - left);
                self.tab.notice = Some(grid.width_text(column));
                changed
            }
            GridHandle::Thumb { axis, grab } => {
                let Some(grid) = self.results_grid() else {
                    return false;
                };
                let shift = self.results_shift();
                let Some(track) = grid_track(grid, axis, self.centre_right()) else {
                    return false;
                };
                let track = shifted(track, shift);
                let extent = grid_extent(grid, axis, self.centre_right() - SIDEBAR_RIGHT);
                let (pointer, length) = along(axis, track, x, y);
                let offset = scrollbar::dragged_offset(pointer, grab, 0.0, length, extent);
                self.set_grid_offset(axis, offset)
            }
            GridHandle::Done => false,
        }
    }

    /// The button went up: a resize or a thumb drag ends.
    pub fn release_grid_handle(&mut self) {
        self.resizing = false;
        if self.dragged_splitter.take().is_some() {
            self.sizes_changed = true;
        }
    }
}

fn popovers(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    if let (Some(view), Some(rect)) = (&window.tab.value_view, window.value_view_rect()) {
        value_view::draw(paint, view, rect);
    }
    if let (Some(menu), Some(rect)) = (&window.tab.grid_menu, window.grid_menu_rect()) {
        let hovered = window.pointer.hovered.and_then(grid_menu_item_of);
        grid_menu::draw(paint, menu, rect, hovered);
    }
    if let (Some(menu), Some(rect)) = (&window.tree_menu, window.tree_menu_rect()) {
        let hovered = window.pointer.hovered.and_then(tree_menu::item_of_node);
        tree_menu::draw(paint, menu, rect, hovered);
    }
    if let (Some(menu), Some(rect)) = (&window.tab.editor_menu, window.editor_menu_rect()) {
        let hovered = window.pointer.hovered.and_then(editor_menu::item_of_node);
        editor_menu::draw(paint, menu, rect, hovered);
    }
}

fn popover_nodes(nodes: &mut Vec<AccessNode>, window: &MainWindow) {
    let open = window.tab.value_view.is_some()
        || window.tab.grid_menu.is_some()
        || window.tab.editor_menu.is_some()
        || window.tree_menu.is_some();
    if !open {
        return;
    }
    for node in nodes.iter_mut() {
        node.state.focused = false;
    }
    if let (Some(view), Some(rect)) = (&window.tab.value_view, window.value_view_rect()) {
        value_view::nodes(nodes, view, rect, NODE_GRID);
    }
    if let (Some(menu), Some(rect)) = (&window.tab.grid_menu, window.grid_menu_rect()) {
        grid_menu::nodes(nodes, menu, rect, NODE_GRID);
    }
    if let (Some(menu), Some(rect)) = (&window.tree_menu, window.tree_menu_rect()) {
        tree_menu::nodes(nodes, menu, rect, NODE_MAIN_WINDOW);
    }
    if let (Some(menu), Some(rect)) = (&window.tab.editor_menu, window.editor_menu_rect()) {
        editor_menu::nodes(nodes, menu, rect, NODE_EDITOR);
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
/// The status bar's `Works like <product>` item, while a familiar layout is active.
pub const NODE_LAYOUT_ITEM: u32 = 63_100;
/// The first tab's close button; the others follow in the strip's order.
pub const NODE_FIRST_TAB_CLOSE: u32 = 61_500;
/// The tab strip's `+` button.
pub const NODE_NEW_TAB: u32 = 61_999;
/// A keys tab's `Key prefix` field (`docs/ux/workspace.md` 10.10).
pub const NODE_KEY_PREFIX: u32 = 63_200;
/// A keys tab's `Back` button.
pub const NODE_KEYS_BACK: u32 = 63_201;
/// A keys tab's `Next 100` button.
pub const NODE_KEYS_NEXT: u32 = 63_202;
const TAB_CLOSE_NODES_END: u32 = NODE_FIRST_TAB_CLOSE + tab_strip::MOST_TABS as u32;

fn node(
    id: u32,
    parent: u32,
    role: AccessRole,
    name: &str,
    bounds: (f32, f32, f32, f32),
) -> AccessNode {
    AccessNode {
        id,
        parent: Some(parent),
        role,
        name: name.to_owned(),
        bounds: AccessRect {
            x: bounds.0,
            y: bounds.1,
            width: bounds.2,
            height: bounds.3,
        },
        ..AccessNode::default()
    }
}

fn primary_press(letter: char) -> KeyPress {
    KeyPress {
        key: Key::Character(letter),
        modifiers: eggdb_platform::window::Modifiers {
            control: true,
            ..Default::default()
        },
    }
}

pub(crate) fn primary_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Cmd"
    } else {
        "Ctrl"
    }
}

fn toolbar_action(id: u32) -> Option<LayoutAction> {
    match id {
        NODE_RUN => Some(LayoutAction::RunStatement),
        NODE_RUN_ALL => Some(LayoutAction::RunAll),
        NODE_EXPLAIN => Some(LayoutAction::Explain),
        _ => None,
    }
}

fn toolbar_shortcut(id: u32, layout: Layout) -> Option<String> {
    if id == NODE_STOP {
        return Some("Esc".to_owned());
    }
    layout.shortcut_label(toolbar_action(id)?)
}

fn toolbar_chord(id: u32, layout: Layout) -> Option<KeyPress> {
    if id == NODE_STOP {
        return Some(KeyPress::plain(Key::Escape));
    }
    Some(layout.key_for(toolbar_action(id)?))
}

fn layout_item_chord() -> [KeyPress; 2] {
    [
        KeyPress {
            key: Key::Character('k'),
            modifiers: crate::accessibility::primary(),
        },
        KeyPress::plain(Key::Character('l')),
    ]
}

fn editor_caret(editor: &crate::editor::Editor) -> AccessCaret {
    let lines = editor.lines();
    let offset =
        |at: crate::editor::Position| crate::accessibility::char_offset(lines, at.line, at.column);
    let caret = editor.caret();
    let selection = editor
        .selection()
        .map(|(start, end)| (offset(start), offset(end)));
    crate::accessibility::field_caret(offset(caret), selection)
}

impl MainWindow {
    /// The plan node under the pointer while the plan's tree shows (`docs/ux/workspace.md`
    /// 5.4).
    pub fn hovered_plan_node(&self) -> Option<usize> {
        let plan = self
            .tab
            .plan
            .as_ref()
            .filter(|plan| self.tab.view == ResultView::Plan && !plan.text_shown)?;
        let index =
            usize::try_from(self.pointer.hovered?.checked_sub(NODE_FIRST_PLAN_NODE)?).ok()?;
        (index < plan.plan.nodes.len()).then_some(index)
    }

    /// Moves the keyboard focus to the element of accessibility node `node`, as when a screen
    /// reader focuses it: the editor, the dialect pill, the filter, a tree row, a grid cell or a
    /// plan node; returns whether that node can take the focus.
    pub fn focus_node(&mut self, node: &AccessNode) -> bool {
        let id = node.id;
        if (NODE_FIRST_BACKGROUND_TAB..NODE_FIRST_TAB_CLOSE).contains(&id) {
            let Some(position) = tab_position_of(self, id) else {
                return false;
            };
            self.show_tab(position);
            return true;
        }
        match id {
            NODE_EDITOR | NODE_QUERY_TAB => self.focus_tab(),
            NODE_DIALECT => {
                self.focus = WindowFocus::Tab;
                self.tab.focus(TabFocus::DialectPill);
            }
            NODE_FILTER => {
                self.focus_tree();
                self.focus = WindowFocus::Filter;
            }
            _ if (NODE_FIRST_TREE_ROW..NODE_FIRST_TREE_ROW + ROWS_IN_VIEW as u32).contains(&id) => {
                let offset = self.tree.first_row + (id - NODE_FIRST_TREE_ROW) as usize;
                let Some(row) = self.tree.rows().into_iter().nth(offset) else {
                    return false;
                };
                self.focus_tree();
                self.tree.current = row.item;
            }
            _ if (NODE_FIRST_PLAN_NODE..NODE_FIRST_PLAN_NODE + plan_view::MAX_NODES as u32)
                .contains(&id) =>
            {
                let index = (id - NODE_FIRST_PLAN_NODE) as usize;
                if self
                    .tab
                    .plan
                    .as_ref()
                    .is_none_or(|plan| index >= plan.plan.nodes.len())
                {
                    return false;
                }
                self.focus = WindowFocus::Tab;
                self.tab.view = ResultView::Plan;
                self.tab.focus(TabFocus::Plan);
                if let Some(plan) = &mut self.tab.plan {
                    plan.current = index;
                }
            }
            _ if node.role == AccessRole::Cell => {
                let Some(at) = node.cell else {
                    return false;
                };
                if self
                    .tab
                    .grid
                    .as_ref()
                    .is_none_or(|grid| at.row >= grid.rows.len() || at.column >= grid.columns.len())
                {
                    return false;
                }
                self.focus = WindowFocus::Tab;
                self.tab.view = ResultView::Results;
                self.tab.focus(TabFocus::Grid);
                if let Some(grid) = &mut self.tab.grid {
                    let cell = crate::grid::Cell {
                        row: at.row,
                        column: at.column,
                    };
                    grid.current = cell;
                    grid.anchor = cell;
                }
            }
            _ => return false,
        }
        true
    }

    /// The keys that do a screen reader's `action` on `node`, after moving the focus there
    /// ([`MainWindow::focus_node`]): the toolbar's buttons give their shortcut from the editor,
    /// and the editor takes a new text or caret at once; nothing when the node cannot do it.
    pub fn access_keys(&mut self, node: &AccessNode, action: &AccessAction) -> Vec<KeyPress> {
        if let Some(picker) = &mut self.layout_picker {
            return layout_picker::access_keys(picker, node, action);
        }
        if node.id == NODE_LAYOUT_ITEM {
            return if *action == AccessAction::Invoke {
                layout_item_chord().to_vec()
            } else {
                Vec::new()
            };
        }
        if let Some((prompt, _)) = &mut self.drop_prompt {
            return match node.id {
                drop_prompt::NODE_DROP_STATEMENT => {
                    prompt.focus = drop_prompt::DropFocus::Statement;
                    Vec::new()
                }
                drop_prompt::NODE_DROP_CONFIRM if prompt.can_drop() => {
                    prompt.focus = drop_prompt::DropFocus::Drop;
                    vec![KeyPress::plain(Key::Enter)]
                }
                drop_prompt::NODE_DROP_CONFIRM | drop_prompt::NODE_DROP_DIALOG => Vec::new(),
                drop_prompt::NODE_DROP_CANCEL => {
                    prompt.focus = drop_prompt::DropFocus::Cancel;
                    vec![KeyPress::plain(Key::Enter)]
                }
                _ => vec![KeyPress::plain(Key::Escape)],
            };
        }
        if self.close_prompt.is_some() {
            if node.id == tab_strip::NODE_CLOSE_CONFIRM {
                if let Some(prompt) = &mut self.close_prompt {
                    prompt.default_focused = true;
                }
                return vec![KeyPress::plain(Key::Enter)];
            }
            return vec![KeyPress::plain(Key::Escape)];
        }
        if let Some(keys) = self.popover_access_keys(node, action) {
            return keys;
        }
        let invoked = *action == AccessAction::Invoke;
        if node.id == NODE_NEW_TAB {
            return if invoked {
                vec![primary_press('t')]
            } else {
                Vec::new()
            };
        }
        if (NODE_FIRST_TAB_CLOSE..TAB_CLOSE_NODES_END).contains(&node.id) {
            let Some(position) = tab_position_of(self, node.id).filter(|_| invoked) else {
                return Vec::new();
            };
            self.show_tab(position);
            return vec![primary_press('w')];
        }
        if let Some(keys) = self.keys_header_access(node.id, action) {
            return keys;
        }
        if let Some(chord) = toolbar_chord(node.id, self.layout) {
            if *action != AccessAction::Invoke {
                return Vec::new();
            }
            self.focus_tab();
            return vec![chord];
        }
        if !self.focus_node(node) {
            return Vec::new();
        }
        match action {
            AccessAction::SetValue(text) if node.id == NODE_EDITOR => {
                self.tab.editor = crate::editor::Editor::new(text);
                Vec::new()
            }
            AccessAction::SetCaret(caret) if node.id == NODE_EDITOR => {
                let lines = self.tab.editor.lines();
                let (line, column) = crate::accessibility::line_and_column(lines, caret.anchor);
                let length = crate::accessibility::char_offset(lines, usize::MAX, usize::MAX);
                let (anchor, to) = (caret.anchor.min(length), caret.caret.min(length));
                self.tab
                    .editor
                    .place_caret(crate::editor::Position { line, column });
                let shift = KeyPress {
                    key: if to >= anchor { Key::Right } else { Key::Left },
                    modifiers: eggdb_platform::window::Modifiers {
                        shift: true,
                        ..Default::default()
                    },
                };
                std::iter::repeat_n(shift, to.abs_diff(anchor)).collect()
            }
            _ => crate::accessibility::action_keys(node, action),
        }
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
    let width = window.width as f32;
    let centre_width = window.centre_right() - 260.0;
    let tab_focused = window.focus == WindowFocus::Tab;
    let mut root = node(
        NODE_MAIN_WINDOW,
        0,
        AccessRole::Window,
        "EggDB",
        (0.0, 0.0, width, 800.0),
    );
    root.parent = None;
    let toolbar_width = window.area_width(Area::Toolbar);
    let dx = run_button_extra(window.layout);
    let mut nodes = vec![
        root,
        node(
            NODE_TOOLBAR,
            NODE_MAIN_WINDOW,
            AccessRole::Toolbar,
            "Toolbar",
            (0.0, 0.0, toolbar_width, 44.0),
        ),
    ];
    for (id, name, x, w) in [
        (NODE_RUN, window.layout.run_label(), 232.0, 72.0 + dx),
        (NODE_RUN_ALL, "Run all", 312.0 + dx, 88.0),
        (NODE_EXPLAIN, "Explain", 408.0 + dx, 84.0),
        (NODE_STOP, "Stop", 500.0 + dx, 68.0),
    ] {
        let mut button = node(
            id,
            NODE_TOOLBAR,
            AccessRole::Button,
            name,
            (x, 8.0, w, 28.0),
        );
        button.state.disabled = if id == NODE_STOP {
            !tab.running
        } else {
            tab.running
        };
        button.shortcut = toolbar_shortcut(id, window.layout);
        button.actions.invoke = true;
        nodes.push(button);
    }
    let mut pill = node(
        NODE_DIALECT,
        NODE_TOOLBAR,
        AccessRole::ComboBox,
        "Dialect",
        (toolbar_width - 224.0, 8.0, 120.0, 28.0),
    );
    pill.value = Some(tab.pill_label().to_owned());
    pill.state.disabled = tab.is_keys();
    pill.state.expanded = Some(tab.dialect_menu.is_some());
    pill.state.focused = tab_focused && tab.focus == TabFocus::DialectPill;
    nodes.push(pill);
    sidebar_nodes(&mut nodes, window);
    nodes.push(node(
        NODE_QUERIES,
        NODE_MAIN_WINDOW,
        AccessRole::TabList,
        "Queries",
        (260.0, 44.0, centre_width, 36.0),
    ));
    tab_nodes(&mut nodes, window, centre_width);
    let mut text = node(
        NODE_EDITOR,
        NODE_MAIN_WINDOW,
        AccessRole::TextArea,
        "Query",
        (260.0, 80.0, centre_width, 300.0),
    );
    text.value = Some(tab.editor.text());
    text.caret = Some(editor_caret(&tab.editor));
    text.state.invalid = tab.error.is_some();
    text.state.focused = tab_focused && tab.focus == TabFocus::Editor;
    nodes.push(text);
    if let Some(error) = tab.error.as_ref().filter(|error| error.strip_shown) {
        nodes.push(node(
            NODE_ERROR_STRIP,
            NODE_MAIN_WINDOW,
            AccessRole::Alert,
            &error.strip_text(),
            (260.0, 352.0, centre_width, 28.0),
        ));
    }
    nodes.push(node(
        NODE_RESULT_VIEWS,
        NODE_MAIN_WINDOW,
        AccessRole::TabList,
        "Result views",
        (260.0, 386.0, centre_width, 32.0),
    ));
    for (offset, (name, view, x, underline)) in
        result_segments(tab, window.layout).into_iter().enumerate()
    {
        let mut segment = node(
            NODE_RESULTS_SEGMENT + offset as u32,
            NODE_RESULT_VIEWS,
            AccessRole::Tab,
            name,
            (x, 386.0, segment_width(window.layout, underline), 32.0),
        );
        segment.state.selected = tab.view == view;
        nodes.push(segment);
    }
    nodes.push(node(
        NODE_STATUS_LINE,
        NODE_MAIN_WINDOW,
        AccessRole::Status,
        &status_line::spoken(&tab.summary),
        (summary_x(tab, window.layout), 386.0, 760.0, 32.0),
    ));
    if tab.is_keys() {
        keys_header_nodes(&mut nodes, tab, window.centre_right());
    }
    if let Some(plan) = tab
        .plan
        .as_ref()
        .filter(|plan| tab.view == ResultView::Plan && !plan.text_shown)
    {
        plan_nodes(
            &mut nodes,
            plan,
            tab_focused && tab.focus == TabFocus::Plan,
            centre_width,
        );
    }
    if let Some(grid) = tab
        .grid
        .as_ref()
        .filter(|_| tab.view == ResultView::Results)
    {
        grid_nodes(&mut nodes, grid, centre_width);
    }
    if window.inspector.is_open() {
        inspector_nodes(&mut nodes, window);
    }
    let status_width = window.area_width(Area::StatusBar);
    let mut bar = node(
        NODE_STATUS_BAR,
        NODE_MAIN_WINDOW,
        AccessRole::Group,
        "Status",
        (0.0, 776.0, status_width, 24.0),
    );
    bar.value = Some(status_bar_left(tab));
    nodes.push(bar);
    if let Some(item) = window.layout.status_item() {
        let mut button = node(
            NODE_LAYOUT_ITEM,
            NODE_STATUS_BAR,
            AccessRole::Button,
            item,
            (
                status_width - LAYOUT_ITEM_RIGHT - LAYOUT_ITEM_WIDTH,
                776.0,
                LAYOUT_ITEM_WIDTH,
                24.0,
            ),
        );
        button.description = Some(crate::layout::FULL_TRADEMARK_NOTE.to_owned());
        button.shortcut = Some(format!("{}+K L", primary_name()));
        button.actions.invoke = true;
        nodes.push(button);
    }
    window.place_nodes(&mut nodes[1..]);
    crate::accessibility::fill_default_actions(&mut nodes);
    for node in nodes.iter_mut().filter(|node| no_keys_reach_it_yet(node)) {
        node.actions = AccessActions::default();
    }
    popover_nodes(&mut nodes, window);
    if let Some(prompt) = &window.close_prompt {
        for node in &mut nodes {
            node.state.focused = false;
        }
        tab_strip::prompt_nodes(&mut nodes, prompt, NODE_MAIN_WINDOW, (width, HEIGHT as f32));
    }
    if let Some((prompt, _)) = &window.drop_prompt {
        for node in &mut nodes {
            node.state.focused = false;
        }
        drop_prompt::nodes(&mut nodes, prompt, NODE_MAIN_WINDOW, (width, HEIGHT as f32));
    }
    if let Some(picker) = &window.layout_picker {
        for node in &mut nodes {
            node.state.focused = false;
        }
        nodes.extend(layout_picker::accessibility_nodes(
            picker,
            NODE_MAIN_WINDOW,
            (width, HEIGHT as f32),
        ));
    }
    nodes
}

const LAYOUT_ITEM_RIGHT: f32 = 240.0;
const LAYOUT_ITEM_WIDTH: f32 = 260.0;

fn tab_nodes(nodes: &mut Vec<AccessNode>, window: &MainWindow, centre_width: f32) {
    let count = window.tab_count();
    let width = tab_strip::tab_width(count, centre_width);
    let viewport = tab_strip::viewport_width(count, centre_width);
    for position in 0..count {
        let Some(tab) = window.tab_at(position) else {
            continue;
        };
        let shown = position == window.tab_position;
        let id = if shown {
            NODE_QUERY_TAB
        } else {
            background_tab_node(window, position)
        };
        let left = tab_left(window, position, width);
        let visible_left = left.max(260.0);
        let visible_right = (left + width).min(260.0 + viewport);
        let mut item = node(
            id,
            NODE_QUERIES,
            AccessRole::Tab,
            &tab.spoken_title(),
            (
                visible_left,
                44.0,
                (visible_right - visible_left).max(0.0),
                36.0,
            ),
        );
        item.state.selected = shown;
        nodes.push(item);
        let close_left = left + width - 28.0;
        if close_left >= 260.0 && close_left + 24.0 <= 260.0 + viewport {
            let mut close = node(
                close_node(position),
                id,
                AccessRole::Button,
                &format!("Close {}", tab.title),
                (close_left, 50.0, 24.0, 24.0),
            );
            close.actions.invoke = true;
            nodes.push(close);
        }
    }
    let mut plus = node(
        NODE_NEW_TAB,
        NODE_QUERIES,
        AccessRole::Button,
        "New query tab",
        (
            tab_strip::plus_x(count, centre_width, 260.0),
            48.0,
            28.0,
            28.0,
        ),
    );
    plus.shortcut = Some(format!("{}+T", primary_name()));
    plus.actions.invoke = true;
    nodes.push(plus);
}

/// The strip position of the tab whose node, or whose close button's node, is `id`.
pub fn tab_position_for_node(window: &MainWindow, id: u32) -> Option<usize> {
    tab_position_of(window, id)
}

fn tab_position_of(window: &MainWindow, id: u32) -> Option<usize> {
    if id == NODE_QUERY_TAB {
        return Some(window.tab_position);
    }
    if (NODE_FIRST_TAB_CLOSE..TAB_CLOSE_NODES_END).contains(&id) {
        let position = (id - NODE_FIRST_TAB_CLOSE) as usize;
        return (position < window.tab_count()).then_some(position);
    }
    if (NODE_FIRST_BACKGROUND_TAB..NODE_FIRST_TAB_CLOSE).contains(&id) {
        let index = (id - NODE_FIRST_BACKGROUND_TAB) as usize;
        if index >= window.background_tabs.len() {
            return None;
        }
        return Some(if index >= window.tab_position {
            index + 1
        } else {
            index
        });
    }
    None
}

fn no_keys_reach_it_yet(node: &AccessNode) -> bool {
    let result_segment = (NODE_RESULTS_SEGMENT..NODE_PLAN).contains(&node.id);
    let inspector_part = node.parent == Some(NODE_INSPECTOR);
    result_segment || inspector_part
}

fn plan_nodes(nodes: &mut Vec<AccessNode>, plan: &PlanView, focused: bool, centre_width: f32) {
    let area = plan_area(centre_width);
    let mut tree = node(
        NODE_PLAN,
        NODE_MAIN_WINDOW,
        AccessRole::Tree,
        "Plan",
        (area.x, area.y, area.w, area.h),
    );
    let count = plan.plan.nodes.len();
    tree.description = Some(format!(
        "{} {}",
        status_line::thousands(count),
        if count == 1 { "node" } else { "nodes" }
    ));
    nodes.push(tree);
    for (index, plan_node) in plan
        .plan
        .nodes
        .iter()
        .enumerate()
        .take(plan_view::MAX_NODES)
    {
        let Some(rect) = plan.node_rect(index, area) else {
            continue;
        };
        let mut item = node(
            NODE_FIRST_PLAN_NODE + index as u32,
            NODE_PLAN,
            AccessRole::TreeItem,
            &plan_node.operation,
            (rect.x, rect.y, rect.w, rect.h),
        );
        item.description = Some(plan.plan.spoken(index));
        let siblings = plan_node
            .parent
            .and_then(|parent| plan.plan.nodes.get(parent))
            .map(|parent| parent.children.as_slice());
        item.tree_position = Some(AccessTreePosition {
            level: plan_node.depth + 1,
            position: siblings
                .and_then(|list| list.iter().position(|&child| child == index))
                .map_or(1, |at| at + 1),
            set_size: siblings.map_or(1, <[usize]>::len),
        });
        item.state.expanded = (!plan_node.children.is_empty()).then_some(true);
        item.actions = AccessActions {
            select: true,
            focus: true,
            ..AccessActions::default()
        };
        item.state.focused = focused && plan.current == index;
        item.state.selected = plan.selected == Some(index);
        nodes.push(item);
    }
}

fn sidebar_nodes(nodes: &mut Vec<AccessNode>, window: &MainWindow) {
    let tree = &window.tree;
    let mut filter = node(
        NODE_FILTER,
        NODE_MAIN_WINDOW,
        AccessRole::TextField,
        "Filter objects",
        (8.0, 52.0, 244.0, 28.0),
    );
    filter.value = Some(tree.filter.clone());
    filter.placeholder = Some("Filter objects".to_owned());
    filter.state.focused = window.focus == WindowFocus::Filter;
    nodes.push(filter);
    nodes.push(node(
        NODE_OBJECTS,
        NODE_MAIN_WINDOW,
        AccessRole::Tree,
        window.layout.sidebar_title(),
        (0.0, ROWS_TOP, 260.0, 776.0 - ROWS_TOP),
    ));
    let rows = tree.rows();
    for (offset, row) in rows
        .iter()
        .enumerate()
        .skip(tree.first_row)
        .take(ROWS_IN_VIEW)
    {
        let visible = offset - tree.first_row;
        let y = ROWS_TOP + ROW_HEIGHT * visible as f32;
        let detail = if row.item == TreeItem::Connection {
            Some(access_text(window).to_owned())
        } else {
            row.detail.clone()
        };
        let mut item = node(
            NODE_FIRST_TREE_ROW + visible as u32,
            NODE_OBJECTS,
            AccessRole::TreeItem,
            &row.label,
            (4.0, y, 252.0, ROW_HEIGHT),
        );
        item.description = Some(object_browser::row_description(row, detail.as_deref()));
        item.tree_position = Some(AccessTreePosition {
            level: row.level + 1,
            position: row.position,
            set_size: row.set_size,
        });
        item.state.expanded = row.expanded;
        item.state.selected = tree.selected == Some(row.item);
        item.state.focused = window.focus == WindowFocus::Tree
            && tree.current == row.item
            && !window.inspector.ddl_focused;
        nodes.push(item);
    }
}

fn inspector_nodes(nodes: &mut Vec<AccessNode>, window: &MainWindow) {
    let area = inspector::region(window.width as f32, window.sizes.inspector_width);
    let description = inspector::describe(
        &window.tree,
        window.inspected(),
        window.inspector.sample.as_ref(),
    );
    let mut panel = node(
        NODE_INSPECTOR,
        NODE_MAIN_WINDOW,
        AccessRole::Group,
        "Inspector",
        (area.x, area.y, area.w, area.h),
    );
    panel.value = Some(description.name.clone());
    nodes.push(panel);
    let mut heading = node(
        NODE_INSPECTOR_NAME,
        NODE_INSPECTOR,
        AccessRole::Heading,
        &description.name,
        (area.x + 12.0, area.y + 12.0, area.w - 24.0, 20.0),
    );
    heading.value = Some(description.subtitle.clone());
    nodes.push(heading);
    for (index, (block, title, top)) in inspector::heading_tops(&description, &window.inspector)
        .into_iter()
        .enumerate()
    {
        let open = window.inspector.is_expanded(block);
        let mut button = node(
            NODE_FIRST_BLOCK + index as u32,
            NODE_INSPECTOR,
            AccessRole::Button,
            &title,
            (area.x + 12.0, top, area.w - 24.0, 24.0),
        );
        button.value = Some(if open { "expanded" } else { "collapsed" }.to_owned());
        button.state.expanded = Some(open);
        nodes.push(button);
        if block == InspectorBlock::Ddl && open {
            let mut ddl = node(
                NODE_DDL,
                NODE_INSPECTOR,
                AccessRole::TextArea,
                "DDL",
                (area.x + 12.0, top + 28.0, area.w - 24.0, 120.0),
            );
            ddl.value = description.ddl.clone();
            ddl.state.read_only = true;
            ddl.state.focused = window.inspector.ddl_focused;
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
            &grid.banner_text(),
            (260.0, 418.0, centre_width, 28.0),
        ));
        top = 446.0;
    }
    let (rows, columns) = grid.selection_size();
    let mut table = node(
        NODE_GRID,
        NODE_MAIN_WINDOW,
        AccessRole::Grid,
        "Results",
        (260.0, top, centre_width, BODY_BOTTOM - top),
    );
    table.table = Some(AccessTable {
        rows: grid.row_count,
        columns: grid.columns.len(),
    });
    table.description = Some(format!(
        "{} rows, {} columns; {} by {} selected",
        grid.row_count,
        grid.columns.len(),
        status_line::rows_text(rows),
        if columns == 1 {
            "1 column".to_owned()
        } else {
            format!("{columns} columns")
        }
    ));
    nodes.push(table);
    let edges = grid.column_edges();
    let visible = visible_columns(&grid.widths, grid.scroll_x, centre_width - 48.0);
    let x_of =
        |column: usize| GRID_LEFT + edges.get(column).copied().unwrap_or(0.0) - grid.scroll_x;
    for column in visible.clone() {
        let name = grid.columns.get(column).map_or("", String::as_str);
        let width = x_of(column + 1) - x_of(column);
        let mut header = node(
            NODE_FIRST_HEADER + column as u32 % 800,
            NODE_GRID,
            AccessRole::ColumnHeader,
            name,
            (x_of(column), top, width, 28.0),
        );
        header.cell = Some(AccessCell { row: 0, column });
        header.description = Some(format!(
            "width {:.0} px",
            grid.widths.get(column).copied().unwrap_or(0.0)
        ));
        nodes.push(header);
    }
    let body_top = top + 28.0;
    let mut id = NODE_FIRST_CELL;
    for row in visible_rows(grid.rows.len(), grid.first_row, BODY_BOTTOM - body_top) {
        for column in visible.clone() {
            let cell = crate::grid::Cell { row, column };
            let y = body_top + 24.0 * (row - grid.first_row) as f32;
            let name = grid.columns.get(column).map_or("", String::as_str);
            let mut item = node(
                id,
                NODE_GRID,
                AccessRole::Cell,
                name,
                (x_of(column), y, x_of(column + 1) - x_of(column), 24.0),
            );
            item.value = Some(spoken_cell(grid.value(cell), grid.kind(column)));
            item.cell = Some(AccessCell { row, column });
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
    fn elided_keeps_what_fits_and_scans_a_long_text_only_as_far_as_it_shows() {
        let face = BoxFace;
        let size = 10.0;
        let advance = face.advance('a', size);
        assert_eq!(elided(&face, "abc", advance * 3.0, size), "abc");
        let shown = elided(&face, "abcdef", advance * 3.0, size);
        assert!(
            shown.ends_with('…') && shown.len() < "abcdef".len() + 3,
            "{shown}"
        );
        assert!(face.width(&shown, size) <= advance * 3.0);
        let long = "é".repeat(1 << 20);
        let cut = elided(&face, &long, advance * 4.0, size);
        assert!(cut.ends_with('…'));
        assert!(elided(&face, &long, f32::MAX, size).chars().count() <= ELIDE_SCAN_LIMIT);
        assert_eq!(elided(&face, "abc", 0.0, size), "");
    }

    #[test]
    fn wheel_steps_take_one_notch_as_three_rows_and_add_up_touchpad_parts() {
        let mut carry = (0.0, 0.0);
        assert_eq!(wheel_steps(&mut carry, 1.0, 0.0), (3, 0));
        assert_eq!(wheel_steps(&mut carry, -1.0, 0.0), (-3, 0));
        assert_eq!(wheel_steps(&mut carry, 0.2, 0.0), (0, 0));
        assert_eq!(wheel_steps(&mut carry, 0.2, 0.0), (1, 0));
        assert_eq!(wheel_steps(&mut carry, f32::NAN, f32::INFINITY), (0, 0));
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
        assert_eq!(
            canvas.pixel(1200, 42 + 28),
            Some(theme.bg2),
            "the highlighted row"
        );
        assert_eq!(canvas.pixel(1200, 42), Some(theme.bg0), "the first row");
        assert_eq!(
            canvas.pixel(1200, 40 + 140 + 2),
            Some(theme.border),
            "the shadow under the last row"
        );
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
        assert_eq!(
            open_statement("orders", QueryDialect::PostgreSql),
            "SELECT * FROM orders LIMIT 100;"
        );
        assert_eq!(
            open_statement("Order Lines", QueryDialect::MySql),
            "SELECT * FROM `Order Lines` LIMIT 100;"
        );
        assert_eq!(
            open_statement("orders", QueryDialect::SqlServer),
            "SELECT TOP 100 * FROM orders;"
        );
        assert_eq!(
            open_statement("orders", QueryDialect::Oracle),
            "SELECT * FROM \"orders\" FETCH FIRST 100 ROWS ONLY;"
        );
        assert_eq!(
            open_statement("ORDERS", QueryDialect::Oracle),
            "SELECT * FROM ORDERS FETCH FIRST 100 ROWS ONLY;"
        );
    }

    #[test]
    fn enter_in_the_tree_opens_query_2_and_runs_it() {
        let mut window = MainWindow::sample();
        let now = Instant::now();
        let _ = window.key(
            KeyPress {
                key: Key::Character('1'),
                modifiers: eggdb_platform::window::Modifiers {
                    control: true,
                    ..Default::default()
                },
            },
            now,
        );
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
        let canvas = draw(
            &window,
            Theme::of(crate::color::ThemeKind::Light),
            box_faces(),
        );
        assert_eq!(
            canvas.pixel(450, 45),
            Some(Theme::of(crate::color::ThemeKind::Light).accent),
            "the second tab is the active one"
        );
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
        assert_eq!(
            canvas.pixel(1400, 790),
            Some(theme.bg2),
            "the status bar spans the window"
        );
        let tree = accessibility_tree(&window);
        assert!(tree.iter().any(|node| node.id == NODE_INSPECTOR));
        let editor = tree.iter().find(|node| node.id == NODE_EDITOR).unwrap();
        assert_eq!(editor.bounds.width, 900.0);
        window.set_width(1280);
        assert!(!window.inspector.is_open());
        assert!(
            !accessibility_tree(&window)
                .iter()
                .any(|node| node.id == NODE_INSPECTOR)
        );
    }

    #[test]
    fn the_sidebar_rows_are_tree_items_with_their_state() {
        let window = MainWindow::sample();
        let tree = accessibility_tree(&window);
        let rows: Vec<&AccessNode> = tree
            .iter()
            .filter(|node| node.parent == Some(NODE_OBJECTS))
            .collect();
        let names: Vec<&str> = rows.iter().map(|node| node.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "shop",
                "Tables",
                "customers",
                "orders",
                "products",
                "Views",
                "Indexes",
                "Keys"
            ]
        );
        assert!(rows[2].state.selected && !rows[2].state.focused);
        assert_eq!(
            rows[1].description.as_deref(),
            Some("3, level 2, 1 of 4, expanded")
        );
        assert_eq!(rows[1].role, AccessRole::TreeItem);
        assert_eq!(
            rows[1].tree_position,
            Some(AccessTreePosition {
                level: 2,
                position: 1,
                set_size: 4
            })
        );
        assert_eq!(rows[1].state.expanded, Some(true));
        assert!(
            tree.iter()
                .filter(|node| node.state.focused)
                .all(|node| node.id == NODE_EDITOR)
        );
    }

    #[test]
    fn the_dump_shows_the_workspace_values_states_actions_and_grid() {
        let dump = crate::accessibility::describe(&accessibility_tree(&MainWindow::sample()));
        let primary = primary_name();
        for line in [
            "  toolbar \"Toolbar\"\n".to_owned(),
            format!("    button \"Run\" actions=invoke shortcut={primary}+Enter\n"),
            "    button \"Stop\" [disabled] actions=invoke shortcut=Esc\n".to_owned(),
            "  grid \"Results\" description=\"3 rows, 2 columns; 1 row by 1 column selected\" table=3x2\n".to_owned(),
            "    columnheader \"id\" description=\"width 80 px\" cell=0,0\n".to_owned(),
            "    cell \"name\" value=\"Ada Lovelace\" actions=select,focus cell=0,1\n".to_owned(),
        ] {
            assert!(dump.contains(&line), "{line} in\n{dump}");
        }
        let combo = dump
            .lines()
            .find(|line| line.contains("combobox \"Dialect\""))
            .unwrap_or_default();
        assert!(
            combo.contains("[collapsed] actions=expand,focus"),
            "{combo}"
        );
        let editor = dump
            .lines()
            .find(|line| line.contains("textarea \"Query\""))
            .unwrap_or_default();
        assert!(
            editor.contains(" caret=")
                && editor.contains("[focused]")
                && editor.contains("actions=set-value,focus"),
            "{editor}"
        );
        let row = dump
            .lines()
            .find(|line| line.contains("treeitem \"Tables\""))
            .unwrap_or_default();
        assert!(
            row.contains("[expanded] actions=invoke,expand,select,focus level=2 1/4"),
            "{row}"
        );
    }

    #[test]
    fn screen_reader_requests_move_the_focus_and_press_the_same_keys() {
        let mut window = MainWindow::sample();
        let tree = accessibility_tree(&window);
        let find = |id: u32| tree.iter().find(|node| node.id == id).cloned().unwrap();
        let cell = tree
            .iter()
            .find(|node| {
                node.cell == Some(AccessCell { row: 1, column: 1 }) && node.role == AccessRole::Cell
            })
            .cloned()
            .unwrap();
        assert!(window.access_keys(&cell, &AccessAction::Focus).is_empty());
        assert_eq!(window.tab.focus, TabFocus::Grid);
        assert_eq!(
            window.tab.grid.as_ref().map(|grid| grid.current),
            Some(crate::grid::Cell { row: 1, column: 1 })
        );
        let run = window.access_keys(&find(NODE_RUN), &AccessAction::Invoke);
        assert_eq!(run, vec![toolbar_chord(NODE_RUN, Layout::EggDb).unwrap()]);
        assert_eq!(
            (window.focus, window.tab.focus),
            (WindowFocus::Tab, TabFocus::Editor)
        );
        assert!(
            window
                .access_keys(&find(NODE_RUN), &AccessAction::Focus)
                .is_empty()
        );
        let editor = find(NODE_EDITOR);
        assert!(
            window
                .access_keys(&editor, &AccessAction::SetValue("SELECT 1".to_owned()))
                .is_empty()
        );
        assert_eq!(window.tab.editor.text(), "SELECT 1");
        let caret = AccessCaret {
            anchor: 0,
            caret: 6,
        };
        let keys = window.access_keys(&editor, &AccessAction::SetCaret(caret));
        assert_eq!(keys.len(), 6);
        for press in keys {
            let _ = window.key(press, Instant::now());
        }
        assert_eq!(editor_caret(&window.tab.editor), caret);
        let filter = find(NODE_FILTER);
        assert!(window.access_keys(&filter, &AccessAction::Focus).is_empty());
        assert_eq!(window.focus, WindowFocus::Filter);
        let tables = tree
            .iter()
            .find(|node| node.name == "Tables" && node.role == AccessRole::TreeItem)
            .cloned()
            .unwrap();
        assert_eq!(
            window.access_keys(&tables, &AccessAction::Collapse),
            vec![KeyPress::plain(Key::Left)]
        );
        assert_eq!(window.focus, WindowFocus::Tree);
        assert!(!window.focus_node(&find(NODE_STATUS_BAR)));
    }

    #[test]
    fn the_tree_names_the_grid_its_headers_and_its_cells() {
        let mut window = MainWindow::sample();
        let tree = accessibility_tree(&window);
        let grid = tree.iter().find(|node| node.id == NODE_GRID).unwrap();
        assert_eq!(
            grid.description.as_deref(),
            Some("3 rows, 2 columns; 1 row by 1 column selected")
        );
        assert_eq!(
            grid.table,
            Some(AccessTable {
                rows: 3,
                columns: 2
            })
        );
        let headers: Vec<&str> = tree
            .iter()
            .filter(|node| node.role == AccessRole::ColumnHeader)
            .map(|node| node.name.as_str())
            .collect();
        assert_eq!(headers, ["id", "name"]);
        let first = tree
            .iter()
            .find(|node| node.id == NODE_FIRST_CELL + 1)
            .unwrap();
        assert_eq!(first.value.as_deref(), Some("Ada Lovelace"));
        assert_eq!(first.cell, Some(AccessCell { row: 0, column: 1 }));
        let status = tree
            .iter()
            .find(|node| node.id == NODE_STATUS_LINE)
            .unwrap();
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
        assert_eq!(
            spoken_cell(Some("\\x0102"), ValueKind::Blob),
            "blob, 2 bytes"
        );
        assert_eq!(first_visible_line(0, 14), 0);
        assert_eq!(first_visible_line(12, 14), 3);
    }

    fn laid_out(layout: Layout) -> MainWindow {
        MainWindow {
            layout,
            ..MainWindow::sample()
        }
    }

    fn bounds(window: &MainWindow, id: u32) -> (f32, f32, f32, f32) {
        let tree = accessibility_tree(window);
        let found = tree
            .iter()
            .find(|node| node.id == id)
            .expect("the node is in the tree");
        (
            found.bounds.x,
            found.bounds.y,
            found.bounds.width,
            found.bounds.height,
        )
    }

    fn faces() -> Faces<'static> {
        Faces {
            ui: &BoxFace,
            ui_bold: &BoxFace,
            mono: &BoxFace,
        }
    }

    #[test]
    fn a_familiar_layout_moves_the_regions_and_their_nodes_to_its_rects() {
        let ssms = laid_out(Layout::Ssms);
        assert_eq!(ssms.centre_right(), 260.0 + 980.0);
        assert_eq!(bounds(&ssms, NODE_EDITOR), (300.0, 80.0, 980.0, 300.0));
        assert_eq!(
            bounds(&ssms, NODE_RESULT_VIEWS),
            (300.0, 426.0, 980.0, 32.0)
        );
        assert_eq!(bounds(&ssms, NODE_QUERIES), (300.0, 44.0, 980.0, 36.0));
        let run = bounds(&ssms, NODE_RUN);
        assert_eq!((run.0, run.2), (232.0, 96.0));
        assert_eq!(bounds(&ssms, NODE_RUN_ALL).0, 336.0);
        let eggdb = MainWindow::sample();
        assert_eq!(bounds(&eggdb, NODE_EDITOR), (260.0, 80.0, 1020.0, 300.0));
        assert_eq!(bounds(&eggdb, NODE_RUN), (232.0, 8.0, 72.0, 28.0));
        assert!(
            accessibility_tree(&eggdb)
                .iter()
                .all(|node| node.id != NODE_LAYOUT_ITEM)
        );
        let tree = accessibility_tree(&ssms);
        let item = tree
            .iter()
            .find(|node| node.id == NODE_LAYOUT_ITEM)
            .expect("the status bar names the layout");
        assert_eq!(item.name, "Works like SQL Server Management Studio");
        assert_eq!(
            item.description.as_deref(),
            Some(crate::layout::FULL_TRADEMARK_NOTE)
        );
        let names: Vec<&str> = tree
            .iter()
            .filter(|node| node.parent == Some(NODE_RESULT_VIEWS))
            .map(|node| node.name.as_str())
            .collect();
        assert_eq!(names[..2], ["Results", "Messages"]);
        let objects = tree
            .iter()
            .find(|node| node.id == NODE_OBJECTS)
            .expect("tree");
        assert_eq!(objects.name, "Object Explorer");
        let compass = laid_out(Layout::Compass);
        assert_eq!(bounds(&compass, NODE_STATUS_BAR).2, 0.0);
        assert_eq!(bounds(&compass, NODE_DIALECT).0, 300.0 + 980.0 - 224.0);
        assert_eq!(bounds(&compass, NODE_EDITOR), (300.0, 108.0, 980.0, 92.0));
        let redis = laid_out(Layout::RedisInsight);
        assert_eq!(bounds(&redis, NODE_QUERIES).2, 0.0);
        let names: Vec<String> = accessibility_tree(&laid_out(Layout::PgAdmin))
            .into_iter()
            .filter(|node| node.parent == Some(NODE_RESULT_VIEWS))
            .map(|node| node.name)
            .collect();
        assert_eq!(names[0], "Data Output");
    }

    #[test]
    fn a_familiar_layout_draws_its_regions_where_its_rects_are() {
        let theme = Theme::of(ThemeKind::Light);
        let eggdb = draw(&MainWindow::sample(), theme, faces());
        let ssms = draw(&laid_out(Layout::Ssms), theme, faces());
        assert_ne!(eggdb, ssms);
        assert_eq!(ssms.pixel(280, 600), Some(theme.bg1));
        assert_eq!(ssms.pixel(299, 600), Some(theme.border));
        assert_eq!(ssms.pixel(640, 423), Some(theme.bg2));
        let workbench = draw(&laid_out(Layout::Workbench), theme, faces());
        assert_eq!(workbench.pixel(900, 700), Some(theme.bg1));
        let compass = draw(&laid_out(Layout::Compass), theme, faces());
        assert_eq!(compass.pixel(150, 790), Some(theme.bg1));
        assert_eq!(
            draw(&MainWindow::sample(), theme, faces()),
            eggdb,
            "EggDB's own frame is the same frame as before"
        );
    }

    #[test]
    fn ctrl_k_l_opens_the_picker_and_its_keys_switch_the_layout_keeping_the_tab() {
        let mut window = MainWindow::sample();
        window.tab.editor = crate::editor::Editor::new("SELECT 1;");
        let now = Instant::now();
        let chord = layout_item_chord();
        assert_eq!(
            window.key(chord[0], now),
            WindowAction::Tab(TabAction::None)
        );
        assert!(window.chord_started && window.layout_picker.is_none());
        window.key(chord[1], now);
        assert!(!window.chord_started);
        assert_eq!(
            window.layout_picker.map(|picker| picker.highlighted),
            Some(0)
        );
        assert_eq!(
            window.key(KeyPress::plain(Key::Escape), now),
            WindowAction::Tab(TabAction::None)
        );
        assert!(window.layout_picker.is_none());
        window.key(chord[0], now);
        window.key(KeyPress::plain(Key::Right), now);
        assert!(window.layout_picker.is_none() && !window.chord_started);
        window.open_layout_picker();
        window.key(KeyPress::plain(Key::Down), now);
        assert_eq!(
            window.key(KeyPress::plain(Key::Enter), now),
            WindowAction::LayoutChosen(Layout::Ssms)
        );
        assert_eq!(window.layout, Layout::Ssms);
        assert_eq!(window.tab.editor.text(), "SELECT 1;");
        window.open_layout_picker();
        let tree = accessibility_tree(&window);
        assert!(
            tree.iter()
                .any(|node| node.id == layout_picker::NODE_DIALOG)
        );
        assert!(
            tree.iter()
                .filter(|node| node.id < layout_picker::NODE_DIALOG)
                .all(|node| !node.state.focused)
        );
        let theme = Theme::of(ThemeKind::Dark);
        let covered = draw(&window, theme, faces());
        assert_ne!(
            covered.pixel(20, 20),
            draw(&laid_out(Layout::Ssms), theme, faces()).pixel(20, 20)
        );
        let item = AccessNode {
            id: NODE_LAYOUT_ITEM,
            ..AccessNode::default()
        };
        window.layout_picker = None;
        assert_eq!(
            window.access_keys(&item, &AccessAction::Invoke),
            chord.to_vec()
        );
    }

    #[test]
    fn a_layout_s_keys_reach_the_actions_from_the_editor_only() {
        let mut window = laid_out(Layout::Ssms);
        window.tab.editor = crate::editor::Editor::new("SELECT 1;");
        let now = Instant::now();
        let WindowAction::Tab(TabAction::Run(request)) = window.key(KeyPress::plain(Key::F5), now)
        else {
            panic!("F5 executes in the SQL Server Management Studio layout");
        };
        assert!(request.all && request.statements.len() == 1);
        let shortcut = accessibility_tree(&window)
            .into_iter()
            .find(|node| node.id == NODE_RUN_ALL)
            .and_then(|node| node.shortcut);
        assert_eq!(shortcut.as_deref(), Some("F5"));
    }

    fn primary(letter: char) -> KeyPress {
        KeyPress {
            key: Key::Character(letter),
            modifiers: eggdb_platform::window::Modifiers {
                control: true,
                ..Default::default()
            },
        }
    }

    fn control_tab(shift: bool) -> KeyPress {
        KeyPress {
            key: Key::Tab,
            modifiers: eggdb_platform::window::Modifiers {
                control: true,
                shift,
                ..Default::default()
            },
        }
    }

    #[test]
    fn ctrl_t_adds_the_lowest_free_query_after_the_active_tab_and_ctrl_tab_wraps() {
        let now = Instant::now();
        let mut window = MainWindow::sample();
        window.tab.set_dialect(QueryDialect::MySql);
        let _ = window.key(primary('t'), now);
        let _ = window.key(primary('t'), now);
        assert_eq!(window.tab_titles(), ["Query 1", "Query 2", "Query 3"]);
        assert_eq!(window.tab.title, "Query 3");
        assert_eq!(window.tab.dialect, QueryDialect::MySql);
        assert_eq!(window.tab.focus, TabFocus::Editor);
        assert_ne!(window.tab.session, window.background_tabs[0].session);
        window.show_tab(0);
        let _ = window.key(primary('t'), now);
        assert_eq!(
            window.tab_titles(),
            ["Query 1", "Query 4", "Query 2", "Query 3"]
        );
        let _ = window.key(control_tab(true), now);
        assert_eq!(window.tab.title, "Query 1");
        let _ = window.key(control_tab(true), now);
        assert_eq!(
            window.tab.title, "Query 3",
            "Ctrl+Shift+Tab wraps to the last tab"
        );
        let _ = window.key(control_tab(false), now);
        assert_eq!(window.tab.title, "Query 1");
        assert_eq!(
            window.tab.grid.as_ref().map(|grid| grid.rows.len()),
            Some(3)
        );
    }

    #[test]
    fn ctrl_w_closes_an_empty_tab_at_once_and_asks_before_closing_one_with_text() {
        let now = Instant::now();
        let mut window = MainWindow::sample();
        let _ = window.key(primary('t'), now);
        let session = window.tab.session;
        let closed = window.key(primary('w'), now);
        assert_eq!(
            closed,
            WindowAction::CloseSession {
                session,
                stop: false
            }
        );
        assert_eq!(window.tab_titles(), ["Query 1"]);
        let asked = window.key(primary('w'), now);
        assert_eq!(asked, WindowAction::Tab(TabAction::None));
        let prompt = window.close_prompt.clone().expect("Query 1 holds text");
        assert_eq!(prompt.title, "Close Query 1 without saving?");
        let tree = accessibility_tree(&window);
        let confirm = tree
            .iter()
            .find(|node| node.id == tab_strip::NODE_CLOSE_CONFIRM)
            .expect("the dialog's default button");
        assert_eq!(confirm.name, "Close");
        assert!(confirm.state.focused);
        let _ = window.key(KeyPress::plain(Key::Escape), now);
        assert!(window.close_prompt.is_none());
        assert_eq!(window.tab.title, "Query 1");
        let _ = window.key(primary('w'), now);
        let last = window.key(KeyPress::plain(Key::Enter), now);
        assert!(matches!(
            last,
            WindowAction::CloseSession { stop: false, .. }
        ));
        assert_eq!(window.tab_titles(), ["Query 1"], "the strip is never empty");
        assert!(window.tab.editor.text().is_empty() && window.tab.grid.is_none());
    }

    #[test]
    fn closing_picks_the_tab_to_the_right_else_the_left() {
        let now = Instant::now();
        let mut window = MainWindow::sample();
        let _ = window.key(primary('t'), now);
        let _ = window.key(primary('t'), now);
        window.show_tab(1);
        let _ = window.key(primary('w'), now);
        assert_eq!(window.tab.title, "Query 3");
        let _ = window.key(primary('w'), now);
        assert_eq!(window.tab.title, "Query 1");
        assert!(window.close_prompt.is_none());
        let _ = window.key(primary('w'), now);
        assert!(window.close_prompt.is_some(), "Query 1 holds text");
    }

    #[test]
    fn a_tab_in_a_transaction_shows_its_dot_name_and_status_item_and_locks_the_dialect() {
        let theme = Theme::of(crate::color::ThemeKind::Light);
        let now = Instant::now();
        let mut window = MainWindow::sample();
        window.tab.transaction = eggdb::sql::TransactionStatus::InBlock;
        let tree = accessibility_tree(&window);
        let tab = tree.iter().find(|node| node.id == NODE_QUERY_TAB).unwrap();
        assert_eq!(tab.name, "Query 1, transaction open");
        let canvas = draw(&window, theme, box_faces());
        assert_eq!(canvas.pixel(275, 62), Some(theme.warning));
        assert!(!window.tab.dialect_switch_allowed());
        window.tab.open_dialect_menu();
        assert_eq!(
            window.tab.notice.as_deref(),
            Some("Warning: COMMIT or ROLLBACK first; Query 1 has a transaction open")
        );
        let _ = window.key(KeyPress::plain(Key::Down), now);
        let _ = window.key(KeyPress::plain(Key::Enter), now);
        assert_eq!(window.tab.dialect, QueryDialect::PostgreSql);
        window.tab.dialect_menu = None;
        let _ = window.key(primary('t'), now);
        assert_eq!(
            window
                .other_tab_in_transaction()
                .map(|tab| tab.title.as_str()),
            Some("Query 1")
        );
        assert_eq!(
            transaction_open_in("Query 1"),
            "Transaction open in Query 1"
        );
        let _ = window.key(control_tab(false), now);
        let _ = window.key(primary('w'), now);
        assert_eq!(
            window
                .close_prompt
                .as_ref()
                .map(|prompt| prompt.title.as_str()),
            Some("Close Query 1 and roll back?")
        );
    }

    #[test]
    fn tabs_shrink_then_scroll_and_the_active_one_stays_in_view() {
        let now = Instant::now();
        let mut window = MainWindow::sample();
        for _ in 0..11 {
            let _ = window.key(primary('t'), now);
        }
        assert_eq!(window.tab_count(), 12);
        assert_eq!(window.tab.title, "Query 12");
        assert_eq!(window.tab_scroll, 224.0);
        let tree = accessibility_tree(&window);
        let plus = tree.iter().find(|node| node.id == NODE_NEW_TAB).unwrap();
        assert_eq!(plus.bounds.x, 1244.0);
        let active = tree.iter().find(|node| node.id == NODE_QUERY_TAB).unwrap();
        assert_eq!((active.bounds.x, active.bounds.width), (1136.0, 100.0));
        window.show_tab(0);
        assert_eq!(window.tab_scroll, 0.0);
        assert!(window.scroll_tabs(1.0));
        assert_eq!(window.tab_scroll, 100.0);
        assert!(window.scroll_tabs(9.0));
        assert_eq!(window.tab_scroll, 224.0);
        assert!(!window.scroll_tabs(1.0));
    }

    #[test]
    fn a_window_holds_at_most_400_tabs_each_with_node_ids_of_its_own() {
        let mut window = MainWindow::sample();
        for _ in 1..tab_strip::MOST_TABS {
            window.add_tab();
        }
        assert_eq!(window.tab_count(), tab_strip::MOST_TABS);
        let last = window.tab.title.clone();
        window.add_tab();
        assert_eq!(window.tab_count(), tab_strip::MOST_TABS);
        assert_eq!(window.tab.title, last);
        assert_eq!(window.tab.notice.as_deref(), Some(tab_strip::TOO_MANY_TABS));
        let mut ids: Vec<u32> = (0..window.tab_count())
            .filter(|&position| position != window.tab_position)
            .map(|position| background_tab_node(&window, position))
            .collect();
        ids.extend((0..window.tab_count()).map(close_node));
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count);
        assert!(!ids.contains(&NODE_NEW_TAB));
        for position in 0..window.tab_count() {
            assert_eq!(
                tab_position_of(&window, close_node(position)),
                Some(position)
            );
        }
        window.show_tab(0);
        let first_background = background_tab_node(&window, 1);
        assert_eq!(tab_position_of(&window, first_background), Some(1));
    }

    #[test]
    fn a_background_tab_and_its_close_button_act_through_the_tree() {
        let now = Instant::now();
        let mut window = MainWindow::sample();
        let _ = window.key(primary('t'), now);
        let tree = accessibility_tree(&window);
        let first = tree
            .iter()
            .find(|node| node.role == AccessRole::Tab && node.name == "Query 1")
            .cloned()
            .unwrap();
        assert!(first.actions.select, "keys reach background tabs now");
        assert!(window.access_keys(&first, &AccessAction::Select).is_empty());
        assert_eq!(window.tab.title, "Query 1");
        let tree = accessibility_tree(&window);
        let close = tree
            .iter()
            .find(|node| node.name == "Close Query 2")
            .cloned()
            .unwrap();
        assert_eq!(
            window.access_keys(&close, &AccessAction::Invoke),
            vec![primary('w')]
        );
        assert_eq!(window.tab.title, "Query 2");
    }

    #[test]
    fn a_right_click_on_a_cell_makes_it_current_and_its_menu_copies_as_csv() {
        let mut window = MainWindow::sample();
        let tree = accessibility_tree(&window);
        assert!(window.context_press(&tree, 508.0, 482.0));
        let grid = window.tab.grid.as_ref().unwrap();
        assert_eq!(grid.current, Cell { row: 1, column: 1 });
        assert_eq!(grid.selection_size(), (1, 1));
        assert_eq!(window.tab.focus, TabFocus::Grid);
        assert_eq!(
            window.grid_menu_rect(),
            Some(Rect::new(508.0, 482.0, 240.0, 278.0))
        );
        let tree = accessibility_tree(&window);
        let item = tree
            .iter()
            .find(|node| node.name == "Copy as CSV")
            .cloned()
            .expect("the menu's items are in the tree");
        assert_eq!(item.parent, Some(grid_menu::NODE_GRID_MENU));
        assert!(window.is_popover_node(item.id));
        assert!(!window.is_popover_node(NODE_EDITOR));
        let keys = window.access_keys(&item, &AccessAction::Invoke);
        assert_eq!(keys, vec![KeyPress::plain(Key::Enter)]);
        let action = window.key(KeyPress::plain(Key::Enter), Instant::now());
        assert_eq!(
            action,
            WindowAction::Tab(TabAction::Copy {
                text: "name\r\nGrace Hopper".to_owned(),
                toast: "Copied 1 cell as CSV".to_owned(),
            })
        );
        assert_eq!(window.tab.grid_menu, None);
    }

    #[test]
    fn a_right_click_on_the_gutter_or_a_header_selects_its_row_or_column() {
        let mut window = MainWindow::sample();
        let tree = accessibility_tree(&window);
        assert!(window.context_press(&tree, 280.0, 500.0));
        let grid = window.tab.grid.as_ref().unwrap();
        assert_eq!(grid.selection(), (2..3, 0..2));
        window.close_popovers();
        let tree = accessibility_tree(&window);
        assert!(window.context_press(&tree, 340.0, 430.0));
        let grid = window.tab.grid.as_ref().unwrap();
        assert_eq!(grid.selection(), (0..3, 0..1));
        assert!(window.press_outside_popover(1000.0, 200.0));
        assert_eq!(window.tab.grid_menu, None);
        let tree = accessibility_tree(&window);
        assert!(!window.context_press(&tree, 700.0, 200.0));
    }

    #[test]
    fn enter_opens_the_value_view_below_the_cell_and_escape_closes_it() {
        let mut window = MainWindow::sample();
        let now = Instant::now();
        window.key(primary_press('3'), now);
        window.key(KeyPress::plain(Key::Down), now);
        window.key(KeyPress::plain(Key::Right), now);
        window.key(KeyPress::plain(Key::Enter), now);
        let view = window
            .tab
            .value_view
            .as_ref()
            .expect("Enter opens the value view");
        assert_eq!(view.cell, Cell { row: 1, column: 1 });
        assert_eq!(view.name(), "name value");
        assert_eq!(
            window.value_view_rect(),
            Some(Rect::new(388.0, 498.0, 480.0, 240.0))
        );
        let tree = accessibility_tree(&window);
        let body = tree
            .iter()
            .find(|node| node.id == value_view::NODE_VALUE_BODY)
            .expect("the body is in the tree");
        assert_eq!(body.value.as_deref(), Some("Grace Hopper"));
        assert!(body.state.focused && body.state.read_only);
        assert_eq!(tree.iter().filter(|node| node.state.focused).count(), 1);
        let theme = Theme::of(ThemeKind::Light);
        let canvas = draw(&window, theme, box_faces());
        assert_eq!(canvas.pixel(600, 600), Some(theme.bg0));
        assert_eq!(canvas.pixel(600, 510), Some(theme.bg1));
        let action = window.key(primary_press('c'), now);
        assert_eq!(
            action,
            WindowAction::Tab(TabAction::Copy {
                text: "Grace Hopper".to_owned(),
                toast: "Copied 12 characters".to_owned(),
            })
        );
        window.key(KeyPress::plain(Key::Escape), now);
        assert_eq!(window.tab.value_view, None);
        assert_eq!(window.tab.focus, TabFocus::Grid);
        let shift_f10 = KeyPress {
            key: Key::F10,
            modifiers: eggdb_platform::window::Modifiers {
                shift: true,
                ..Default::default()
            },
        };
        window.key(shift_f10, now);
        let menu = window.tab.grid_menu.expect("Shift+F10 opens the menu");
        assert_eq!(menu.origin, grid_menu::MenuOrigin::CurrentCell);
        assert_eq!(menu.highlighted, Some(0));
        assert_eq!(
            window.grid_menu_rect(),
            Some(Rect::new(388.0, 498.0, 240.0, 278.0))
        );
    }

    #[test]
    fn a_header_edge_resizes_its_column_by_drag_and_fits_it_on_a_double_click() {
        let mut window = MainWindow::sample();
        assert_eq!(window.column_edge_at(387.0, 430.0), Some(0));
        assert_eq!(window.column_edge_at(390.0, 430.0), Some(0));
        assert_eq!(window.column_edge_at(380.0, 430.0), None);
        assert_eq!(window.column_edge_at(307.0, 430.0), None);
        assert_eq!(window.column_edge_at(387.0, 460.0), None);
        assert!(window.hover_grid_handles(387.0, 430.0));
        assert_eq!(window.column_edge, Some(0));
        let handle = window.press_grid_handle(388.0, 430.0, 1);
        assert_eq!(
            handle,
            Some(GridHandle::ColumnEdge {
                column: 0,
                grab: 0.0
            })
        );
        let handle = handle.unwrap();
        assert!(window.drag_grid_handle(handle, 500.0, 600.0));
        assert_eq!(window.tab.grid.as_ref().unwrap().widths[0], 192.0);
        assert_eq!(window.tab.notice.as_deref(), Some("id · 192 px"));
        window.drag_grid_handle(handle, 2_000.0, 600.0);
        assert_eq!(window.tab.grid.as_ref().unwrap().widths[0], 960.0);
        window.drag_grid_handle(handle, 500.0, 600.0);
        window.release_grid_handle();
        assert!(!window.resizing);
        assert_eq!(
            window.press_grid_handle(500.0, 430.0, 2),
            Some(GridHandle::Done)
        );
        assert_eq!(window.tab.grid.as_ref().unwrap().widths[0], 48.0);
        let tree = accessibility_tree(&window);
        let header = tree
            .iter()
            .find(|node| node.role == AccessRole::ColumnHeader && node.name == "id")
            .unwrap();
        assert_eq!(header.description.as_deref(), Some("width 48 px"));
    }

    #[test]
    fn alt_keys_widen_narrow_and_fit_the_current_column() {
        let mut window = MainWindow::sample();
        let now = Instant::now();
        window.key(primary_press('3'), now);
        let alt = |key: Key, shift: bool| KeyPress {
            key,
            modifiers: eggdb_platform::window::Modifiers {
                alt: true,
                shift,
                ..Default::default()
            },
        };
        window.key(alt(Key::Right, false), now);
        assert_eq!(window.tab.grid.as_ref().unwrap().widths[0], 88.0);
        assert_eq!(window.tab.notice.as_deref(), Some("id · 88 px"));
        window.key(alt(Key::Left, true), now);
        assert_eq!(window.tab.grid.as_ref().unwrap().widths[0], 56.0);
        window.key(alt(Key::Enter, false), now);
        assert_eq!(window.tab.grid.as_ref().unwrap().widths[0], 48.0);
        assert_eq!(window.tab.value_view, None);
        assert_eq!(window.tab.grid.as_ref().unwrap().current, Cell::default());
    }

    #[test]
    fn scrollbars_show_only_past_the_viewport_and_page_and_drag_the_rows() {
        let window = MainWindow::sample();
        let grid = window.tab.grid.as_ref().unwrap();
        assert_eq!(grid_track(grid, Axis::Vertical, 1280.0), None);
        assert_eq!(grid_track(grid, Axis::Horizontal, 1280.0), None);
        let mut window = MainWindow::sample();
        let rows: Vec<Vec<Option<String>>> = (1..=40)
            .map(|n| vec![Some(n.to_string()), Some(format!("name {n}"))])
            .collect();
        window.tab.grid = Some(ResultGrid::new(
            vec!["id".into(), "name".into()],
            vec![ValueKind::Number, ValueKind::Text],
            rows,
            40,
            vec![80.0, 240.0],
        ));
        let grid = window.tab.grid.as_ref().unwrap();
        assert_eq!(
            grid_track(grid, Axis::Vertical, 1280.0),
            Some(Rect::new(1270.0, 446.0, 10.0, 330.0))
        );
        assert_eq!(grid_track(grid, Axis::Horizontal, 1280.0), None);
        assert_eq!(
            window.press_grid_handle(1275.0, 446.0 + 200.0, 1),
            Some(GridHandle::Done)
        );
        assert_eq!(window.tab.grid.as_ref().unwrap().first_row, 13);
        let handle = window
            .press_grid_handle(1275.0, 446.0 + 117.0, 1)
            .expect("a press on the thumb");
        assert_eq!(
            handle,
            GridHandle::Thumb {
                axis: Axis::Vertical,
                grab: 10.0
            }
        );
        assert!(window.drag_grid_handle(handle, 1275.0, 446.0 + 233.0));
        assert_eq!(window.tab.grid.as_ref().unwrap().first_row, 27);
        window.tab.grid.as_mut().unwrap().widths = vec![600.0, 600.0];
        let grid = window.tab.grid.as_ref().unwrap();
        assert_eq!(
            grid_track(grid, Axis::Horizontal, 1280.0),
            Some(Rect::new(308.0, 766.0, 962.0, 10.0))
        );
        assert_eq!(
            grid_track(grid, Axis::Vertical, 1280.0),
            Some(Rect::new(1270.0, 446.0, 10.0, 320.0))
        );
        let theme = Theme::of(ThemeKind::Light);
        let canvas = draw(&window, theme, box_faces());
        assert_ne!(canvas.pixel(1275, 700), Some(theme.bg0));
    }

    #[test]
    fn the_tree_row_menu_opens_from_the_keyboard_and_the_pointer_and_copies_names() {
        let mut window = MainWindow::sample();
        let now = Instant::now();
        window.focus_tree();
        window.tree.current = TreeItem::Table(1);
        assert_eq!(
            window.key(primary_press('c'), now),
            WindowAction::Tab(TabAction::Copy {
                text: "orders".to_owned(),
                toast: "Copied orders".to_owned(),
            })
        );
        window.key(KeyPress::plain(Key::ContextMenu), now);
        let menu = window.tree_menu.expect("the menu key opens the row menu");
        assert_eq!(menu.origin, TreeMenuOrigin::CurrentRow);
        assert_eq!(menu.highlighted, Some(0));
        assert_eq!(
            window.tree_menu_rect(),
            Some(Rect::new(76.0, 188.0, tree_menu::WIDTH, tree_menu::HEIGHT))
        );
        let nodes = accessibility_tree(&window);
        let drop_item = nodes
            .iter()
            .find(|node| node.id == tree_menu::NODE_FIRST_TREE_MENU_ITEM + 5)
            .expect("the menu's items are in the tree");
        assert_eq!(drop_item.name, "Drop…");
        assert!(!drop_item.state.disabled);
        assert!(window.is_popover_node(drop_item.id));
        window.key(KeyPress::plain(Key::Escape), now);
        assert_eq!(window.tree_menu, None);
        assert_eq!(window.focus, WindowFocus::Tree);
        let nodes = accessibility_tree(&window);
        assert!(window.context_press(&nodes, 100.0, 146.0));
        assert_eq!(window.tree.selected, Some(TreeItem::Table(0)));
        assert_eq!(
            window.tree_menu.map(|menu| menu.origin),
            Some(TreeMenuOrigin::Pointer { x: 100.0, y: 146.0 })
        );
        for _ in 0..4 {
            window.key(KeyPress::plain(Key::Down), now);
        }
        assert_eq!(
            window.key(KeyPress::plain(Key::Enter), now),
            WindowAction::Inspect(TreeItem::Table(0))
        );
        assert!(window.inspector.is_open());
        assert!(window.inspector.is_expanded(InspectorBlock::Ddl));
        let nodes = accessibility_tree(&window);
        assert!(!window.context_press(&nodes, 100.0, 700.0));
        assert_eq!(window.focus, WindowFocus::Tree);
        assert_eq!(window.tree_menu, None);
    }

    #[test]
    fn drop_asks_with_the_tabs_dialect_and_waits_for_a_tab_in_a_transaction() {
        let mut window = MainWindow::sample();
        let now = Instant::now();
        window.tab.set_dialect(QueryDialect::MySql);
        window.focus_tree();
        window
            .tree
            .expanded
            .insert(TreeItem::Group(crate::object_browser::Group::Indexes));
        let index = window
            .tree
            .catalog
            .indexes
            .iter()
            .position(|index| index.name == "orders_customer_id_idx")
            .unwrap();
        window.open_drop_prompt(TreeItem::Index(index));
        let (prompt, then_selected) = window.drop_prompt.clone().expect("the dialog opens");
        assert_eq!(
            prompt.statement,
            "DROP INDEX orders_customer_id_idx ON orders;"
        );
        assert_eq!(prompt.title(), "Drop index orders_customer_id_idx?");
        assert_eq!(
            then_selected,
            TreeItem::Group(crate::object_browser::Group::Indexes)
        );
        window.key(KeyPress::plain(Key::Escape), now);
        assert!(window.drop_prompt.is_none());
        window.add_tab();
        window.tab.transaction = eggdb::sql::TransactionStatus::InBlock;
        window.show_tab(0);
        window.open_drop_prompt(TreeItem::Table(1));
        let (prompt, _) = window.drop_prompt.clone().expect("the dialog opens");
        assert_eq!(prompt.body(), "COMMIT or ROLLBACK in Query 2 first.");
        assert!(!prompt.can_drop());
        for tab in &mut window.background_tabs {
            tab.transaction = eggdb::sql::TransactionStatus::Idle;
        }
        window.refresh_drop_blocked();
        let (prompt, _) = window.drop_prompt.clone().expect("the dialog stays open");
        assert!(prompt.can_drop());
        assert_eq!(
            prompt.body(),
            "Runs at once in its own session; there is no undo."
        );
        window.drop_prompt = None;
        let mut read_only = MainWindow {
            writable: false,
            ..MainWindow::sample()
        };
        read_only.open_drop_prompt(TreeItem::Table(1));
        assert!(read_only.drop_prompt.is_none());
        read_only.open_tree_menu(TreeItem::Table(1), TreeMenuOrigin::CurrentRow);
        assert_eq!(read_only.tree_menu.map(|menu| menu.enabled[5]), Some(false));
    }

    #[test]
    fn a_resp_window_opens_on_its_first_key_space_instead_of_an_empty_query_tab() {
        let catalog = Catalog {
            key_spaces: vec![KeySpace {
                collection: Some("resp0".to_owned()),
                keys: KeyCount::Exactly(3),
            }],
            over_resp: true,
            ..Catalog::default()
        };
        let mut window = MainWindow::with_catalog("cache", "localhost:6379", true, catalog);
        let Some(WindowAction::Tab(TabAction::Run(_))) = window.open_on_first_key_space() else {
            panic!("the first key space is scanned");
        };
        assert!(window.tab.is_keys());
        assert_eq!(window.tab_count(), 1, "no Query 1 beside it");
        assert_eq!(window.tab.session, SessionId(1));
        assert!(window.open_on_first_key_space().is_none(), "only once");
        let mut sql = MainWindow::sample();
        assert!(sql.open_on_first_key_space().is_none());
        assert_eq!(sql.tab.title, "Query 1");
    }

    #[test]
    fn enter_on_a_key_space_opens_its_keys_tab_once_and_scans_it_again() {
        let mut window = MainWindow::sample();
        window.tab.set_dialect(QueryDialect::MySql);
        let WindowAction::Tab(TabAction::Run(request)) = window.open_keys(0) else {
            panic!("a key space opens a keys tab that scans it");
        };
        assert_eq!(window.tab.title, "Keys in sessions");
        assert_eq!(window.tab.editor.text(), "SCAN sessions LIMIT 100");
        assert_eq!(
            request.scan.as_ref().map(|scan| scan.title()).as_deref(),
            Some("Keys in sessions")
        );
        assert_eq!(window.tab.pill_label(), "Keys");
        assert_eq!(
            window.tab.run_request(true),
            None,
            "Run all is disabled in a keys tab"
        );
        assert_eq!(window.tab.explain_request(false), Ok(None));
        assert_eq!(
            window.tab.key(KeyPress::plain(Key::Character('x'))),
            TabAction::None
        );
        assert_eq!(
            window.tab.editor.text(),
            "SCAN sessions LIMIT 100",
            "the editor is read-only"
        );
        assert!(
            ClosePrompt::for_tab(&window.tab).is_none(),
            "a keys tab closes without asking"
        );
        window.show_tab(0);
        assert!(matches!(
            window.open_keys(0),
            WindowAction::Tab(TabAction::Run(_))
        ));
        assert_eq!(
            window.tab_count(),
            2,
            "the second Enter reuses the keys tab"
        );
        assert_eq!(window.tab.title, "Keys in sessions");
        window.add_tab();
        assert_eq!(window.tab.title, "Query 2");
        assert_eq!(
            window.tab.dialect,
            QueryDialect::MySql,
            "a tab added from a keys tab takes the nearest query tab's dialect"
        );
    }

    #[test]
    fn splitters_move_the_regions_and_the_rail_collapses_the_sidebar() {
        let theme = Theme::of(crate::color::ThemeKind::Light);
        let mut window = MainWindow::sample();
        let now = Instant::now();
        let primary = |key: Key| KeyPress {
            key,
            modifiers: eggdb_platform::window::Modifiers {
                control: true,
                ..Default::default()
            },
        };
        window.focus_tree();
        let _ = window.key(KeyPress::plain(Key::Tab), now);
        assert_eq!(window.focused_splitter, Some(Splitter::Sidebar));
        let _ = window.key(KeyPress::plain(Key::Right), now);
        assert_eq!(window.sizes.sidebar_width, 268.0);
        assert!(window.sizes_changed);
        assert_eq!(window.tab.notice.as_deref(), Some("Sidebar · 268 px"));
        assert_eq!(window.centre_right() + 8.0, 1280.0);
        assert_eq!(window.splitter_at(266.0, 300.0), Some(Splitter::Sidebar));
        let _ = window.key(KeyPress::plain(Key::Escape), now);
        assert_eq!(window.focus, WindowFocus::Tree);
        assert_eq!(window.focused_splitter, None);
        let canvas = draw(&window, theme, box_faces());
        assert_eq!(
            canvas.pixel(267, 600),
            Some(theme.border),
            "the sidebar ends at 268 px"
        );
        let handle = window.press_layout_handle(400.0, 382.0, 1);
        assert_eq!(handle, Some(GridHandle::Splitter(Splitter::Results)));
        assert!(window.drag_grid_handle(GridHandle::Splitter(Splitter::Results), 400.0, 300.0));
        assert_eq!(window.sizes.results_band_top(), 297.0);
        window.release_grid_handle();
        assert_eq!(window.dragged_splitter, None);
        assert_eq!(
            window.press_layout_handle(400.0, 299.0, 2),
            Some(GridHandle::Done)
        );
        assert_eq!(
            window.sizes.results_band_top(),
            380.0,
            "a double-click resets it"
        );
        let _ = window.key(primary(Key::Character('b')), now);
        assert!(window.rail_shown() && !window.sizes.sidebar_shown);
        assert_eq!(window.sidebar_right(), RAIL_WIDTH);
        let canvas = draw(&window, theme, box_faces());
        assert_eq!(
            canvas.pixel(35, 600),
            Some(theme.border),
            "the rail is 36 px wide"
        );
        assert_eq!(
            window.press_layout_handle(10.0, 60.0, 1),
            Some(GridHandle::Done)
        );
        assert!(window.drawer_open);
        assert_eq!(window.drawer_from, Some(RailItem::Objects));
        assert_eq!(window.focus, WindowFocus::Tree);
        let _ = draw(&window, theme, box_faces());
        assert_eq!(
            window.press_layout_handle(600.0, 300.0, 1),
            Some(GridHandle::Done)
        );
        assert!(!window.drawer_open, "a click outside closes the drawer");
        let _ = window.key(primary(Key::Character('b')), now);
        assert!(!window.rail_shown());
        assert_eq!(window.sidebar_right(), 268.0);
    }

    #[test]
    fn the_inspector_closes_below_1440_and_reopens_as_it_was_and_windows_go_down_to_800() {
        let mut window = MainWindow::sample();
        let now = Instant::now();
        let control_i = KeyPress {
            key: Key::Character('i'),
            modifiers: eggdb_platform::window::Modifiers {
                control: true,
                ..Default::default()
            },
        };
        window.set_width(1500);
        assert!(window.inspector.docked);
        assert!(window.splitter_band(Splitter::Inspector).is_some());
        window.set_width(1300);
        assert!(!window.inspector.is_open());
        assert!(window.splitter_band(Splitter::Inspector).is_none());
        let _ = window.key(control_i, now);
        assert!(window.inspector.overlay);
        window.set_width(1500);
        assert!(window.inspector.docked && !window.inspector.overlay);
        let _ = window.key(control_i, now);
        assert!(!window.inspector.docked && !window.sizes.inspector_shown);
        window.set_width(1300);
        window.set_width(1500);
        assert!(!window.inspector.docked, "a closed inspector stays closed");
        window.set_width(900);
        assert_eq!(window.width, 900);
        window.set_width(500);
        assert_eq!(window.width, 800);
        let _ = draw(
            &window,
            Theme::of(crate::color::ThemeKind::Light),
            box_faces(),
        );
    }

    #[test]
    fn shift_f10_opens_the_editor_menu_below_the_caret_and_show_ddl_focuses_the_ddl_box() {
        let now = Instant::now();
        let mut window = MainWindow::sample();
        window.tab.editor = crate::editor::Editor::new("select 1");
        window.focus = WindowFocus::Tab;
        window.tab.focus = TabFocus::Editor;
        let shift_f10 = KeyPress {
            key: Key::F10,
            modifiers: eggdb_platform::window::Modifiers {
                shift: true,
                ..Default::default()
            },
        };
        window.key(shift_f10, now);
        let menu = window
            .tab
            .editor_menu
            .expect("Shift+F10 opens the editor menu");
        assert_eq!(menu.highlighted, Some(2), "the first enabled item, Paste");
        let rect = window.editor_menu_rect().expect("the menu has a rect");
        assert_eq!(rect.y, EDITOR_TEXT_TOP + LINE_HEIGHT + 4.0);
        assert_eq!(rect.h, editor_menu::HEIGHT);
        let tree = accessibility_tree(&window);
        let cut = tree
            .iter()
            .find(|node| node.id == editor_menu::NODE_FIRST_EDITOR_MENU_ITEM)
            .expect("Cut is exposed");
        assert_eq!(cut.name, "Cut");
        assert!(cut.state.disabled, "a disabled item stays in the menu");
        assert_eq!(cut.parent, Some(editor_menu::NODE_EDITOR_MENU));
        let run = tree
            .iter()
            .find(|node| node.id == editor_menu::NODE_FIRST_EDITOR_MENU_ITEM + 4)
            .expect("Run is exposed");
        assert_eq!(run.name, "Run statement");
        window.key(KeyPress::plain(Key::Character('f')), now);
        window.key(KeyPress::plain(Key::Enter), now);
        assert_eq!(window.tab.editor_menu, None);
        assert_eq!(window.tab.editor.text(), "SELECT\n  1");
        window.key(KeyPress::plain(Key::ContextMenu), now);
        window.key(KeyPress::plain(Key::Escape), now);
        assert_eq!(window.tab.editor_menu, None, "Esc closes the menu");
        assert_eq!(window.tab.editor.text(), "SELECT\n  1");

        window.show_ddl(TreeItem::Table(1));
        assert!(window.inspector.ddl_focused);
        let tree = accessibility_tree(&window);
        if let Some(ddl) = tree.iter().find(|node| node.id == NODE_DDL) {
            assert!(ddl.state.focused && ddl.state.read_only);
            assert_eq!(ddl.role, AccessRole::TextArea);
            assert!(
                !tree
                    .iter()
                    .any(|node| node.role == AccessRole::TreeItem && node.state.focused),
                "the focus moved off the tree"
            );
        }
        window.key(KeyPress::plain(Key::Escape), now);
        assert!(!window.inspector.ddl_focused);
        assert_eq!(window.focus, WindowFocus::Tree);
    }

    #[test]
    fn a_keys_tab_menu_copies_the_scan_line_and_disables_the_editing_items() {
        let now = Instant::now();
        let mut window = MainWindow::sample();
        window.open_keys(0);
        window.focus = WindowFocus::Tab;
        window.tab.focus = TabFocus::Editor;
        window.key(KeyPress::plain(Key::ContextMenu), now);
        let menu = window
            .tab
            .editor_menu
            .expect("the menu key opens it in a keys tab");
        assert_eq!(menu.run_label, "Run scan");
        assert_eq!(
            menu.enabled,
            [false, true, false, false, true, false, false, false]
        );
        assert_eq!(
            window.editor_menu_rect().map(|rect| rect.x),
            Some(EDITOR_TEXT_LEFT)
        );
        let WindowAction::Tab(TabAction::Copy { text, .. }) =
            window.key(KeyPress::plain(Key::Enter), now)
        else {
            panic!("Copy copies the scan line");
        };
        assert_eq!(text, "SCAN sessions LIMIT 100");
        let tree = accessibility_tree(&window);
        let prefix = tree
            .iter()
            .find(|node| node.id == NODE_KEY_PREFIX)
            .expect("the keys tab has its prefix field");
        assert_eq!(prefix.name, "Key prefix");
        assert_eq!(prefix.role, AccessRole::SearchBox);
        assert_eq!(prefix.placeholder.as_deref(), Some("Prefix"));
        let back = tree
            .iter()
            .find(|node| node.id == NODE_KEYS_BACK)
            .expect("Back is exposed");
        assert!(back.state.disabled, "Back is off on page 1");
        assert_eq!(back.shortcut.as_deref(), Some("Alt+PgUp"));
        let (field, back, next) = keys_header_rects(1280.0);
        assert_eq!(field, Rect::new(884.0, 388.0, 200.0, 28.0));
        assert_eq!(back, Rect::new(1096.0, 388.0, 72.0, 28.0));
        assert_eq!(next, Rect::new(1176.0, 388.0, 92.0, 28.0));
    }
}
