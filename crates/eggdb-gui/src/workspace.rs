use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eggdb::sql::TransactionStatus;
use eggdb_platform::window::{
    AccessAction, AccessNode, AccessRequest, AccessRole, KeyPress, Modifiers, OpenFileDialog,
    PointerButton,
};

use crate::catalog::Catalog;
use crate::clipboard::{Clipboard, MemoryClipboard};
use crate::connect_screen::{self, ConnectAction, ConnectCard, Segment, handle_key};
use crate::connection::{
    BracketedRun, CancelHandle, ConnectFailure, ConnectTarget, Connection, Connector, ResultTable,
    SessionId, StatementFailure,
};
use crate::dialect::QueryDialect;
use crate::inspector;
use crate::layout::Layout;
use crate::layout_picker::{self, LayoutPicker, PickerOutcome};
use crate::main_window::{self, Faces, MainWindow, WindowAction};
use crate::object_browser::TreeItem;
use crate::plan_view;
use crate::pointer::{self, ClickCounter, PointerLook};
use crate::query_tab::{RunRequest, TabAction};
use crate::settings::ClientSettings;
use crate::splitters::RegionSizes;

/// How long an open runs before the card shows its progress state, so a quick open never
/// flickers.
pub const PROGRESS_DELAY: Duration = Duration::from_millis(120);
/// How long `Cancelled` stays on the status line.
pub const CANCELLED_SHOWN_FOR: Duration = Duration::from_secs(2);

/// How long a notice such as `Copied 12 cells` stays in the status bar.
pub const NOTICE_SHOWN_FOR: Duration = Duration::from_secs(2);

type RunAnswer = (
    Connection,
    Vec<Result<Vec<ResultTable>, StatementFailure>>,
    TransactionStatus,
    Option<crate::key_scan::PageEnd>,
);
type OpenAnswer = Result<(Connection, Catalog), ConnectFailure>;
type HandedConnection = (Connection, Vec<SessionId>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum CatalogRead {
    Catalog,
    Sample(String),
    Drop {
        statement: String,
        dialect: QueryDialect,
    },
    CloseSessions(Vec<SessionId>),
}

enum CatalogAnswer {
    Catalog(Result<Catalog, StatementFailure>),
    Sample(inspector::Sample),
    Dropped(Result<(), StatementFailure>),
    SessionsClosed,
}

/// How long the toast after a drop from the tree shows (`docs/ux/workspace.md` 10.2).
pub const DROPPED_SHOWN_FOR: Duration = Duration::from_secs(8);

struct Reading {
    read: CatalogRead,
    answer: Receiver<(Connection, CatalogAnswer)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Drag {
    CardField { box_left: f32 },
    Editor,
    Grid,
    Handle(main_window::GridHandle),
}

/// What the window around the workspace must do after an input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceCommand {
    /// Nothing beyond drawing the new frame.
    None,
    /// Show the OS's open-file dialog and give its answer to [`Workspace::file_chosen`].
    ChooseFile(OpenFileDialog),
    /// Quit the client.
    Quit,
}

/// The screen the workspace shows.
#[derive(Debug)]
pub enum Screen {
    /// The connect card, until a database opens.
    Connect(Box<ConnectCard>),
    /// The main window on the open database.
    Main(Box<MainWindow>),
}

struct Running {
    request: RunRequest,
    started: Instant,
    cancel: CancelHandle,
    answer: Receiver<RunAnswer>,
}

struct Opening {
    target: ConnectTarget,
    started: Instant,
    answer: Receiver<OpenAnswer>,
}

/// The client's state from the connect card to the main window: it turns the card's actions
/// into opens on a [`Connector`], runs each open, run and catalog read off the window's thread
/// so the window never waits on the database, and holds the [`Connection`] the main window
/// works on.
pub struct Workspace {
    screen: Screen,
    connection: Option<Connection>,
    connector: Arc<dyn Connector>,
    opening: Option<Opening>,
    running: Option<Running>,
    reading: Option<Reading>,
    run_waiting_for_connection: Option<mpsc::Sender<HandedConnection>>,
    sessions_to_close: Vec<SessionId>,
    catalog_wanted: bool,
    sample_wanted: Option<String>,
    drop_wanted: Option<(String, QueryDialect)>,
    status_until: Option<Instant>,
    notice_until: Option<Instant>,
    clipboard: Box<dyn Clipboard>,
    home: Option<PathBuf>,
    current: PathBuf,
    clicks: ClickCounter,
    drag: Option<Drag>,
    last_input: Instant,
    caret_blinks: bool,
    window_active: bool,
    reduced_motion: bool,
    fill_targets: (Option<u32>, Option<u32>),
    plan_hover: Option<(usize, Instant)>,
    layout: Layout,
    settings_path: Option<PathBuf>,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Workspace")
            .field("screen", &self.screen)
            .field("connection", &self.connection)
            .field("opening", &self.opening.is_some())
            .finish_non_exhaustive()
    }
}

impl Workspace {
    /// The first screen's card on `connector`, resolving typed paths against `home` and
    /// `current`; `file_dialog` says whether the platform has an open-file dialog.
    pub fn new(
        connector: Arc<dyn Connector>,
        home: Option<PathBuf>,
        current: PathBuf,
        file_dialog: bool,
    ) -> Self {
        let mut card = ConnectCard::first_start();
        card.file_dialog = file_dialog;
        Self {
            screen: Screen::Connect(Box::new(card)),
            connection: None,
            connector,
            opening: None,
            running: None,
            reading: None,
            run_waiting_for_connection: None,
            sessions_to_close: Vec::new(),
            catalog_wanted: false,
            sample_wanted: None,
            drop_wanted: None,
            status_until: None,
            notice_until: None,
            clipboard: Box::new(MemoryClipboard::default()),
            home,
            current,
            clicks: ClickCounter::default(),
            drag: None,
            last_input: Instant::now(),
            caret_blinks: false,
            window_active: true,
            reduced_motion: false,
            fill_targets: (None, None),
            plan_hover: None,
            layout: Layout::EggDb,
            settings_path: None,
        }
    }

    /// Keeps the client's settings in the file at `path` (`None`: nowhere) and shows the
    /// layout they name.
    pub fn use_settings(&mut self, path: Option<PathBuf>) {
        if let Some(layout) = path
            .as_deref()
            .and_then(|path| ClientSettings::load(path).layout)
        {
            self.show_layout(layout);
        }
        self.settings_path = path;
    }

    /// The layout the main window shows, or opens in.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Shows `layout` on the card and in the main window, without keeping it in the settings;
    /// for scripted runs.
    pub fn show_layout(&mut self, layout: Layout) {
        self.layout = layout;
        match &mut self.screen {
            Screen::Connect(card) => card.layout = layout,
            Screen::Main(window) => window.layout = layout,
        }
    }

    fn keep_layout(&mut self, layout: Layout, now: Instant) {
        self.show_layout(layout);
        let sizes = self.saved_sizes(layout);
        if let Some(window) = self.main_window_mut() {
            window.use_sizes(sizes);
        }
        let Some(path) = self.settings_path.clone() else {
            return;
        };
        let mut settings = ClientSettings::load(&path);
        settings.layout = Some(layout);
        if let Err(error) = settings.save(&path) {
            let notice = format!("{} applies to this window only: {error}", layout.label());
            match &mut self.screen {
                Screen::Connect(card) => {
                    card.status = Some(notice);
                    self.status_until = Some(now + NOTICE_SHOWN_FOR);
                }
                Screen::Main(window) => {
                    window.tab.notice = Some(notice);
                    self.notice_until = Some(now + NOTICE_SHOWN_FOR);
                }
            }
        }
    }

    fn picker_outcome(&mut self, outcome: PickerOutcome, now: Instant) {
        let chosen = match &mut self.screen {
            Screen::Connect(card) => match outcome {
                PickerOutcome::Open => None,
                PickerOutcome::Keep => {
                    card.layout_picker = None;
                    None
                }
                PickerOutcome::Apply(layout) => {
                    card.layout_picker = None;
                    Some(layout)
                }
            },
            Screen::Main(window) => match window.picker_outcome(outcome) {
                WindowAction::LayoutChosen(layout) => Some(layout),
                _ => None,
            },
        };
        if let Some(layout) = chosen {
            self.keep_layout(layout, now);
        }
    }

    fn picker_mut(&mut self) -> Option<&mut LayoutPicker> {
        match &mut self.screen {
            Screen::Connect(card) => card.layout_picker.as_mut(),
            Screen::Main(window) => window.layout_picker.as_mut(),
        }
    }

    fn picker_size(&self, size: (u32, u32)) -> (f32, f32) {
        match &self.screen {
            Screen::Connect(_) => (size.0 as f32, size.1 as f32),
            Screen::Main(window) => (window.width as f32, main_window::HEIGHT as f32),
        }
    }

    /// Makes the caret blink (`docs/ux/workspace.md` 6.2) from `now`, as a window does;
    /// offscreen frames keep it solid.
    pub fn start_caret_blink(&mut self, now: Instant) {
        self.caret_blinks = true;
        self.last_input = now;
    }

    /// Keeps the caret solid and every fade instant, as the system's reduced-motion setting
    /// asks.
    pub fn set_reduced_motion(&mut self, reduced: bool) {
        self.reduced_motion = reduced;
        if reduced {
            self.pointer_look_mut().stop_fades();
        }
    }

    fn tick_fades(&mut self, now: Instant) -> bool {
        let before = self.fill_targets;
        let reduced = self.reduced_motion;
        let look = self.pointer_look_mut();
        let targets = (look.hovered, look.pressed);
        if reduced {
            look.stop_fades();
        } else if targets != before {
            look.follow(before, now);
        }
        let changed = look.advance(now);
        self.fill_targets = targets;
        changed
    }

    fn tick_plan_tooltip(&mut self, now: Instant) -> bool {
        let hovered = match &self.screen {
            Screen::Main(window) => window.hovered_plan_node(),
            Screen::Connect(_) => None,
        };
        match (self.plan_hover, hovered) {
            (Some((node, _)), Some(current)) if node == current => {}
            (_, current) => self.plan_hover = current.map(|node| (node, now)),
        }
        let due = self
            .plan_hover
            .filter(|&(_, since)| now.saturating_duration_since(since) >= plan_view::TOOLTIP_DELAY)
            .map(|(node, _)| node);
        self.main_window_mut()
            .and_then(|window| window.tab.plan.as_mut())
            .is_some_and(|plan| plan.show_pointer_tooltip(due))
    }

    /// How long until the pointer has rested on a plan node long enough for its tooltip,
    /// `None` when it rests on none or the tooltip already shows (`docs/ux/workspace.md` 5.4).
    pub fn until_plan_tooltip(&self, now: Instant) -> Option<Duration> {
        let (node, since) = self.plan_hover?;
        let Screen::Main(window) = &self.screen else {
            return None;
        };
        let shown = window
            .tab
            .plan
            .as_ref()
            .is_some_and(|plan| plan.tooltip.is_some_and(|(shown, _)| shown == node));
        if shown {
            return None;
        }
        Some(plan_view::TOOLTIP_DELAY.saturating_sub(now.saturating_duration_since(since)))
    }

    /// How long until a fading fill needs a new frame, `None` when nothing fades
    /// (`docs/ux/workspace.md` 10.8).
    pub fn until_fade_changes(&self) -> Option<Duration> {
        let look = match &self.screen {
            Screen::Connect(card) => &card.pointer,
            Screen::Main(window) => &window.pointer,
        };
        look.is_fading().then_some(pointer::FADE_FRAME_INTERVAL)
    }

    /// The window became active (`true`) or inactive: an inactive window's caret is solid
    /// `textDisabled`. Returns whether the frame changed.
    pub fn set_window_active(&mut self, active: bool, now: Instant) -> bool {
        self.window_active = active;
        self.refresh_caret(now)
    }

    fn caret_since(&self, now: Instant) -> Duration {
        if self.caret_blinks {
            now.saturating_duration_since(self.last_input)
        } else {
            Duration::ZERO
        }
    }

    fn refresh_caret(&mut self, now: Instant) -> bool {
        let look = pointer::caret_look(
            self.caret_since(now),
            self.reduced_motion,
            self.window_active,
        );
        let slot = match &mut self.screen {
            Screen::Connect(card) => &mut card.caret,
            Screen::Main(window) => &mut window.caret,
        };
        let changed = *slot != look;
        *slot = look;
        changed
    }

    /// How long until the caret's blink needs a new frame, `None` when it does not blink.
    pub fn until_caret_changes(&self, now: Instant) -> Option<Duration> {
        if !self.caret_blinks {
            return None;
        }
        pointer::until_caret_changes(
            self.caret_since(now),
            self.reduced_motion,
            self.window_active,
        )
    }

    fn input_at(&mut self, now: Instant) {
        self.last_input = now;
        self.refresh_caret(now);
    }

    fn pointer_tree(&self, faces: Faces<'_>, size: (u32, u32)) -> Vec<AccessNode> {
        match &self.screen {
            Screen::Connect(card) => connect_screen::accessibility_tree(card, faces, size),
            Screen::Main(window) => main_window::accessibility_tree(window),
        }
    }

    fn pointer_look_mut(&mut self) -> &mut PointerLook {
        match &mut self.screen {
            Screen::Connect(card) => &mut card.pointer,
            Screen::Main(window) => &mut window.pointer,
        }
    }

    /// The pointer moved to (`x`, `y`) logical pixels in a window of `size`: the hover moves
    /// to the control under it, and a drag that started in an input, the editor or the grid
    /// extends its selection. Returns whether the frame changed.
    pub fn pointer_moved(&mut self, x: f32, y: f32, faces: Faces<'_>, size: (u32, u32)) -> bool {
        let tree = self.pointer_tree(faces, size);
        let mut changed = match self.drag {
            Some(drag) => self.drag_to(drag, &tree, faces, x, y),
            None => self
                .main_window_mut()
                .is_some_and(|window| window.hover_grid_handles(x, y)),
        };
        let hovered = pointer::hit_test(&tree, x, y).map(|node| node.id);
        let look = self.pointer_look_mut();
        if look.hovered != hovered {
            look.hovered = hovered;
            changed = true;
        }
        changed
    }

    /// The pointer left the window: nothing is hovered. Returns whether the frame changed.
    pub fn pointer_left(&mut self) -> bool {
        let look = self.pointer_look_mut();
        let mut changed = look.hovered.is_some();
        look.hovered = None;
        if let Some(window) = self.main_window_mut().filter(|window| !window.resizing) {
            changed |= window.column_edge.take().is_some() | window.scrollbar_hot.take().is_some();
        }
        changed
    }

    /// A button went down at (`x`, `y`) (`docs/ux/workspace.md` 6.1): the primary button
    /// presses the control under it, focuses an input or the editor with the caret at the
    /// press (the word on a double-click, the line or the value on a triple-click, extending
    /// with `Shift`), sets the grid's current cell (extending with `Shift`), and selects a tree
    /// row (a double-click opens it). Buttons, links, checkboxes and tabs act on release.
    #[allow(clippy::too_many_arguments)]
    pub fn pointer_pressed(
        &mut self,
        x: f32,
        y: f32,
        button: PointerButton,
        modifiers: Modifiers,
        faces: Faces<'_>,
        size: (u32, u32),
        now: Instant,
    ) -> WorkspaceCommand {
        if button == PointerButton::Middle {
            return self.middle_pressed(x, y, faces, size, now);
        }
        if button == PointerButton::Secondary {
            self.secondary_pressed(x, y, faces, size, now);
            return WorkspaceCommand::None;
        }
        if button != PointerButton::Primary || self.card_mut().is_some_and(|card| card.busy) {
            return WorkspaceCommand::None;
        }
        self.input_at(now);
        let clicks = self.clicks.press(now, x, y);
        let logical = self.picker_size(size);
        if let Some(picker) = self.picker_mut() {
            let outcome = layout_picker::press_at(picker, logical, x, y, clicks);
            self.picker_outcome(outcome, now);
            return WorkspaceCommand::None;
        }
        if self
            .main_window_mut()
            .is_some_and(|window| window.press_outside_popover(x, y))
        {
            return WorkspaceCommand::None;
        }
        if let Some(handle) = self.main_window_mut().and_then(|window| {
            window
                .press_layout_handle(x, y, clicks)
                .or_else(|| window.press_grid_handle(x, y, clicks))
        }) {
            self.keep_sizes(now);
            if handle != main_window::GridHandle::Done {
                self.drag = Some(Drag::Handle(handle));
            }
            return WorkspaceCommand::None;
        }
        let tree = self.pointer_tree(faces, size);
        let Some(node) = pointer::hit_test(&tree, x, y)
            .filter(|node| self.popover_allows(node.id))
            .cloned()
        else {
            return WorkspaceCommand::None;
        };
        {
            let look = self.pointer_look_mut();
            look.pressed = Some(node.id);
            look.hovered = Some(node.id);
        }
        let extend = modifiers.shift;
        let request = |action: AccessAction| AccessRequest {
            node: node.id,
            action,
        };
        let mut command = WorkspaceCommand::None;
        match node.role {
            AccessRole::TextField | AccessRole::SearchBox | AccessRole::TextArea => {
                self.access(&request(AccessAction::Focus), &tree, now);
                match &mut self.screen {
                    Screen::Connect(card) => {
                        card.place_caret_at(faces, node.bounds.x, x, clicks, extend);
                        self.drag = Some(Drag::CardField {
                            box_left: node.bounds.x,
                        });
                    }
                    Screen::Main(window) if node.id == main_window::NODE_EDITOR => {
                        window.press_editor(faces, x, y, clicks, extend);
                        self.drag = Some(Drag::Editor);
                    }
                    Screen::Main(_) => {}
                }
            }
            AccessRole::Cell => {
                let anchor = self
                    .main_window_mut()
                    .and_then(|window| window.tab.grid.as_ref().map(|grid| grid.anchor));
                self.access(&request(AccessAction::Focus), &tree, now);
                if extend
                    && let Some(anchor) = anchor
                    && let Some(grid) = self
                        .main_window_mut()
                        .and_then(|window| window.tab.grid.as_mut())
                {
                    grid.anchor = anchor;
                }
                if clicks == 2
                    && !extend
                    && let Some(window) = self.main_window_mut()
                {
                    window.tab.open_value();
                } else {
                    self.drag = Some(Drag::Grid);
                }
            }
            AccessRole::TreeItem => {
                let action = if clicks == 2 {
                    AccessAction::Invoke
                } else {
                    AccessAction::Select
                };
                command = self.access(&request(action), &tree, now);
            }
            _ => {}
        }
        self.pointer_look_mut().focus_by_pointer = true;
        command
    }

    fn popover_allows(&self, id: u32) -> bool {
        match &self.screen {
            Screen::Main(window) => window.popover_rect().is_none() || window.is_popover_node(id),
            Screen::Connect(_) => true,
        }
    }

    fn secondary_pressed(
        &mut self,
        x: f32,
        y: f32,
        faces: Faces<'_>,
        size: (u32, u32),
        now: Instant,
    ) {
        if self.picker_mut().is_some() || !matches!(self.screen, Screen::Main(_)) {
            return;
        }
        self.input_at(now);
        let tree = self.pointer_tree(faces, size);
        let clipboard_has_text = self.clipboard.text().is_some();
        let Some(window) = self.main_window_mut() else {
            return;
        };
        if window.close_prompt.is_some() || window.drop_prompt.is_some() {
            return;
        }
        window.clipboard_has_text = clipboard_has_text;
        if window.editor_context_press(faces, &tree, x, y) {
            return;
        }
        window.context_press(&tree, x, y);
        if window.tree_menu.is_some() {
            let item = window.inspected();
            self.inspect(item);
        }
    }

    fn middle_pressed(
        &mut self,
        x: f32,
        y: f32,
        faces: Faces<'_>,
        size: (u32, u32),
        now: Instant,
    ) -> WorkspaceCommand {
        let tree = self.pointer_tree(faces, size);
        let Some(node) = pointer::hit_test(&tree, x, y) else {
            return WorkspaceCommand::None;
        };
        let parent_tab = tree
            .iter()
            .find(|tab| Some(tab.id) == node.parent && tab.role == AccessRole::Tab);
        let tab = if node.role == AccessRole::Tab {
            Some(node)
        } else {
            parent_tab
        };
        let Some(id) = tab.map(|tab| tab.id) else {
            return WorkspaceCommand::None;
        };
        let Screen::Main(window) = &mut self.screen else {
            return WorkspaceCommand::None;
        };
        let Some(position) = main_window::tab_position_for_node(window, id) else {
            return WorkspaceCommand::None;
        };
        let action = window.close_tab_at(position);
        self.window_action(action, now);
        WorkspaceCommand::None
    }

    /// A button went up at (`x`, `y`): a press that started on a button, link, checkbox,
    /// combo box or tab and ends on it acts (invoke, toggle, select); a release elsewhere
    /// cancels it. A drag ends.
    pub fn pointer_released(
        &mut self,
        x: f32,
        y: f32,
        button: PointerButton,
        faces: Faces<'_>,
        size: (u32, u32),
        now: Instant,
    ) -> WorkspaceCommand {
        if button != PointerButton::Primary {
            return WorkspaceCommand::None;
        }
        self.drag = None;
        if let Some(window) = self.main_window_mut() {
            window.release_grid_handle();
        }
        self.keep_sizes(now);
        let Some(pressed) = self.pointer_look_mut().pressed.take() else {
            return WorkspaceCommand::None;
        };
        let tree = self.pointer_tree(faces, size);
        let Some(node) = pointer::hit_test(&tree, x, y)
            .filter(|node| node.id == pressed)
            .cloned()
        else {
            return WorkspaceCommand::None;
        };
        let action = match node.role {
            AccessRole::Button | AccessRole::Link | AccessRole::ComboBox | AccessRole::MenuItem => {
                AccessAction::Invoke
            }
            AccessRole::CheckBox => AccessAction::Toggle,
            AccessRole::Tab => AccessAction::Select,
            _ => return WorkspaceCommand::None,
        };
        let command = self.access(
            &AccessRequest {
                node: node.id,
                action,
            },
            &tree,
            now,
        );
        self.pointer_look_mut().focus_by_pointer = true;
        command
    }

    /// The wheel scrolled at (`x`, `y`) by `lines_down` and `lines_right`: the grid or the
    /// object tree under the pointer scrolls (`docs/ux/workspace.md` 3.6). Returns whether the
    /// frame changed.
    #[allow(clippy::too_many_arguments)]
    pub fn wheel(
        &mut self,
        x: f32,
        y: f32,
        lines_down: f32,
        lines_right: f32,
        modifiers: Modifiers,
        faces: Faces<'_>,
        size: (u32, u32),
    ) -> bool {
        let tree = self.pointer_tree(faces, size);
        match &mut self.screen {
            Screen::Main(window) => {
                window.wheel(&tree, x, y, lines_down, lines_right, modifiers.shift)
            }
            Screen::Connect(_) => false,
        }
    }

    fn drag_to(
        &mut self,
        drag: Drag,
        tree: &[AccessNode],
        faces: Faces<'_>,
        x: f32,
        y: f32,
    ) -> bool {
        match (drag, &mut self.screen) {
            (Drag::CardField { box_left }, Screen::Connect(card)) => {
                let before = card
                    .focused_field()
                    .map(|field| (field.caret(), field.selection()));
                card.place_caret_at(faces, box_left, x, 1, true);
                card.focused_field()
                    .map(|field| (field.caret(), field.selection()))
                    != before
            }
            (Drag::Editor, Screen::Main(window)) => {
                let before = (window.tab.editor.caret(), window.tab.editor.selection());
                window.drag_editor(faces, x, y);
                (window.tab.editor.caret(), window.tab.editor.selection()) != before
            }
            (Drag::Handle(handle), Screen::Main(window)) => window.drag_grid_handle(handle, x, y),
            (Drag::Grid, Screen::Main(window)) => {
                let Some(at) = tree
                    .iter()
                    .rev()
                    .find(|node| {
                        node.role == AccessRole::Cell && pointer::contains(node.bounds, x, y)
                    })
                    .and_then(|node| node.cell)
                else {
                    return false;
                };
                let Some(grid) = &mut window.tab.grid else {
                    return false;
                };
                let cell = crate::grid::Cell {
                    row: at.row,
                    column: at.column,
                };
                if grid.current == cell
                    || at.row >= grid.rows.len()
                    || at.column >= grid.columns.len()
                {
                    return false;
                }
                grid.current = cell;
                true
            }
            _ => false,
        }
    }

    /// The screen to draw.
    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// The connect card, while it is shown.
    pub fn card_mut(&mut self) -> Option<&mut ConnectCard> {
        match &mut self.screen {
            Screen::Connect(card) => Some(card),
            Screen::Main(_) => None,
        }
    }

    /// The open database, once the main window shows it.
    pub fn connection_mut(&mut self) -> Option<&mut Connection> {
        self.connection.as_mut()
    }

    /// Whether an open is running.
    pub fn is_opening(&self) -> bool {
        self.opening.is_some()
    }

    /// Whether a query runs.
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The clipboard copies go to.
    pub fn clipboard(&self) -> &dyn Clipboard {
        self.clipboard.as_ref()
    }

    /// Sends copies to `clipboard`, as the platform layer's system clipboard.
    pub fn set_clipboard(&mut self, clipboard: Box<dyn Clipboard>) {
        self.clipboard = clipboard;
    }

    /// The main window, once it shows.
    pub fn main_window_mut(&mut self) -> Option<&mut MainWindow> {
        match &mut self.screen {
            Screen::Main(window) => Some(window),
            Screen::Connect(_) => None,
        }
    }

    /// Answers a key press.
    pub fn key(&mut self, press: KeyPress, now: Instant) -> WorkspaceCommand {
        self.input_at(now);
        let card_picked = match &mut self.screen {
            Screen::Connect(card) => card.layout_picker.as_mut().map(|picker| picker.key(press)),
            Screen::Main(_) => None,
        };
        if let Some(outcome) = card_picked {
            self.picker_outcome(outcome, now);
            return WorkspaceCommand::None;
        }
        let menu_key = matches!(
            press.key,
            eggdb_platform::window::Key::F10 | eggdb_platform::window::Key::ContextMenu
        );
        let clipboard_has_text = menu_key.then(|| self.clipboard.text().is_some());
        if let Screen::Main(window) = &mut self.screen {
            if let Some(held) = clipboard_has_text {
                window.clipboard_has_text = held;
            }
            let action = window.key(press, now);
            self.window_action(action, now);
            return WorkspaceCommand::None;
        }
        let Screen::Connect(card) = &mut self.screen else {
            return WorkspaceCommand::None;
        };
        let (next, action) = handle_key(ConnectCard::clone(card), press);
        **card = next;
        self.act(action, now)
    }

    /// Answers a screen reader's request on a node of `tree`, the screen's accessibility tree
    /// as last built: the node takes the focus, then gets the keys a person would press for
    /// the same result, through [`Workspace::key`]. A node that is gone, cannot take the focus
    /// or does not offer the action changes nothing.
    pub fn access(
        &mut self,
        request: &AccessRequest,
        tree: &[AccessNode],
        now: Instant,
    ) -> WorkspaceCommand {
        let Some(node) = tree
            .iter()
            .find(|node| node.id == request.node)
            .filter(|node| node.allows(&request.action))
        else {
            return WorkspaceCommand::None;
        };
        let keys = match &mut self.screen {
            Screen::Connect(card) if card.layout_picker.is_some() => card
                .layout_picker
                .as_mut()
                .map(|picker| layout_picker::access_keys(picker, node, &request.action))
                .unwrap_or_default(),
            Screen::Connect(card) => {
                if !card.focus_node(node.id) {
                    return WorkspaceCommand::None;
                }
                if let AccessAction::SetValue(text) = &request.action
                    && node.role == AccessRole::TextField
                {
                    if card.set_focused_value(text) {
                        self.input_at(now);
                    }
                    return WorkspaceCommand::None;
                }
                crate::accessibility::action_keys(node, &request.action)
            }
            Screen::Main(window) => window.access_keys(node, &request.action),
        };
        let mut command = WorkspaceCommand::None;
        for press in keys {
            let answer = self.key(press, now);
            if answer != WorkspaceCommand::None {
                command = answer;
            }
        }
        command
    }

    fn window_action(&mut self, action: WindowAction, now: Instant) {
        match action {
            WindowAction::Tab(action) => self.tab_action(action, now),
            WindowAction::Inspect(item) => self.inspect(item),
            WindowAction::RefreshCatalog => self.refresh_catalog(),
            WindowAction::Drop { statement, dialect } => {
                self.drop_wanted = Some((statement, dialect));
                self.start_read();
            }
            WindowAction::LayoutChosen(layout) => self.keep_layout(layout, now),
            WindowAction::CloseSession { session, stop } => self.close_session(session, stop),
        }
        self.keep_sizes(now);
    }

    fn keep_sizes(&mut self, now: Instant) {
        let path = self.settings_path.clone();
        let Some(window) = self.main_window_mut() else {
            return;
        };
        if !std::mem::take(&mut window.sizes_changed) {
            return;
        }
        let (sizes, layout) = (window.sizes, window.layout);
        self.notice_until = Some(now + NOTICE_SHOWN_FOR);
        let Some(path) = path else {
            return;
        };
        let mut settings = ClientSettings::load(&path);
        sizes.write_to(&mut settings, layout);
        if let Err(error) = settings.save(&path)
            && let Some(window) = self.main_window_mut()
        {
            window.tab.notice = Some(format!("The new sizes apply to this window only: {error}"));
        }
    }

    fn saved_sizes(&self, layout: Layout) -> RegionSizes {
        self.settings_path
            .as_deref()
            .map_or_else(RegionSizes::default, |path| {
                RegionSizes::from_settings(&ClientSettings::load(path), layout)
            })
    }

    fn close_session(&mut self, session: SessionId, stop: bool) {
        if stop
            && let Some(running) = &self.running
            && running.request.session == session
        {
            running.cancel.cancel();
        }
        self.sessions_to_close.push(session);
        self.start_read();
    }

    fn inspect(&mut self, item: TreeItem) {
        let Screen::Main(window) = &self.screen else {
            return;
        };
        let Some(name) = inspector::sampled_name(&window.tree, item) else {
            return;
        };
        let shown = window
            .inspector
            .sample
            .as_ref()
            .is_some_and(|sample| sample.of == name);
        let being_read = self
            .reading
            .as_ref()
            .is_some_and(|reading| reading.read == CatalogRead::Sample(name.clone()));
        if shown || being_read {
            self.sample_wanted = None;
            return;
        }
        self.sample_wanted = Some(name);
        self.start_read();
    }

    /// Reads the catalog again off the window's thread and shows it in the tree when it
    /// arrives, keeping its expanded, current and selected rows; while a run or another read
    /// holds the connection, the read starts once it is back.
    pub fn refresh_catalog(&mut self) {
        if matches!(self.screen, Screen::Main(_)) {
            self.catalog_wanted = true;
            self.start_read();
        }
    }

    fn start_read(&mut self) {
        if self.reading.is_some() || self.run_waiting_for_connection.is_some() {
            return;
        }
        let read = if !self.sessions_to_close.is_empty() {
            CatalogRead::CloseSessions(std::mem::take(&mut self.sessions_to_close))
        } else if let Some((statement, dialect)) = self.drop_wanted.take() {
            CatalogRead::Drop { statement, dialect }
        } else if self.catalog_wanted {
            self.catalog_wanted = false;
            CatalogRead::Catalog
        } else if let Some(name) = self.sample_wanted.take() {
            CatalogRead::Sample(name)
        } else {
            return;
        };
        let Some(mut connection) = self.connection.take() else {
            match read {
                CatalogRead::Catalog => self.catalog_wanted = true,
                CatalogRead::Sample(name) => self.sample_wanted = Some(name),
                CatalogRead::Drop { statement, dialect } => {
                    self.drop_wanted = Some((statement, dialect));
                }
                CatalogRead::CloseSessions(sessions) => self.sessions_to_close = sessions,
            }
            return;
        };
        let (send, answer) = mpsc::channel();
        let worker_read = read.clone();
        std::thread::spawn(move || {
            let answer = match worker_read {
                CatalogRead::Catalog => CatalogAnswer::Catalog(connection.catalog()),
                CatalogRead::Sample(name) => {
                    CatalogAnswer::Sample(inspector::fetch_sample(&mut connection, &name))
                }
                CatalogRead::Drop { statement, dialect } => {
                    let outcomes =
                        connection.run_script(dialect, &[statement], &CancelHandle::default());
                    CatalogAnswer::Dropped(match outcomes.into_iter().last() {
                        Some(Err(failure)) => Err(failure),
                        _ => Ok(()),
                    })
                }
                CatalogRead::CloseSessions(sessions) => {
                    for session in sessions {
                        connection.close_session(session);
                    }
                    CatalogAnswer::SessionsClosed
                }
            };
            let _ = send.send((connection, answer));
        });
        self.reading = Some(Reading { read, answer });
    }

    fn finish_read(&mut self, connection: Connection, answer: CatalogAnswer) {
        self.reading = None;
        if let Screen::Main(window) = &mut self.screen {
            match answer {
                CatalogAnswer::Catalog(Ok(catalog)) => {
                    window.tree.set_catalog(catalog);
                    window.inspector.sample = None;
                }
                CatalogAnswer::Catalog(Err(_)) => {}
                CatalogAnswer::Sample(sample) => window.inspector.sample = Some(sample),
                CatalogAnswer::Dropped(Ok(())) => {
                    if window.drop_succeeded().is_some() {
                        self.notice_until = Some(Instant::now() + DROPPED_SHOWN_FOR);
                    }
                    window.inspector.sample = None;
                    self.catalog_wanted = true;
                }
                CatalogAnswer::Dropped(Err(failure)) => window.drop_failed(&failure),
                CatalogAnswer::SessionsClosed => {}
            }
        }
        self.connection_back(connection);
    }

    fn connection_back(&mut self, connection: Connection) {
        match self.run_waiting_for_connection.take() {
            Some(run) => {
                let handed = run.send((connection, std::mem::take(&mut self.sessions_to_close)));
                if let Err(mpsc::SendError((connection, to_close))) = handed {
                    self.sessions_to_close = to_close;
                    self.connection = Some(connection);
                    self.start_read();
                }
            }
            None => {
                self.connection = Some(connection);
                self.start_read();
            }
        }
    }

    fn tick_read(&mut self) -> bool {
        let Some(reading) = &self.reading else {
            return false;
        };
        match reading.answer.try_recv() {
            Ok((connection, answer)) => {
                self.finish_read(connection, answer);
                true
            }
            Err(TryRecvError::Disconnected) => {
                self.reading = None;
                false
            }
            Err(TryRecvError::Empty) => false,
        }
    }

    /// Waits up to `limit` for the catalog and sample reads in flight or waiting for the
    /// connection, and shows their answers; for scripted runs and tests.
    pub fn wait_for_reads(&mut self, limit: Duration) {
        let until = Instant::now() + limit;
        while let Some(reading) = &self.reading {
            let left = until.saturating_duration_since(Instant::now());
            match reading.answer.recv_timeout(left) {
                Ok((connection, answer)) => self.finish_read(connection, answer),
                Err(mpsc::RecvTimeoutError::Disconnected) => self.reading = None,
                Err(mpsc::RecvTimeoutError::Timeout) => return,
            }
        }
    }

    fn tab_action(&mut self, action: TabAction, now: Instant) {
        match action {
            TabAction::None => {
                if self
                    .main_window_mut()
                    .is_some_and(|window| window.tab.notice.is_some())
                {
                    self.notice_until.get_or_insert(now + NOTICE_SHOWN_FOR);
                }
            }
            TabAction::Run(request) => self.start_run(*request, now),
            TabAction::Stop => {
                if let Some(running) = &self.running {
                    running.cancel.cancel();
                }
                if let Some(window) = self.main_window_mut() {
                    window.tab.stop_requested();
                }
            }
            TabAction::Copy { text, toast } => {
                self.clipboard.put_text(&text);
                if let Some(window) = self.main_window_mut() {
                    window.tab.notice = Some(toast);
                }
                self.notice_until = Some(now + NOTICE_SHOWN_FOR);
            }
            TabAction::Paste => {
                let pasted = self.clipboard.text().map(|raw| pointer::paste_text(&raw));
                let Some(window) = self.main_window_mut() else {
                    return;
                };
                match pasted {
                    Some(Ok(text)) => window.tab.paste(&text),
                    Some(Err(toast)) => {
                        window.tab.notice = Some(toast.to_owned());
                        self.notice_until = Some(now + NOTICE_SHOWN_FOR);
                    }
                    None => {}
                }
            }
        }
    }

    fn start_run(&mut self, request: RunRequest, now: Instant) {
        if let Some(running) = &self.running {
            let session = running.request.session;
            if let Some(window) = self.main_window_mut() {
                let title = window
                    .tab_with_session_mut(session)
                    .map(|tab| tab.title.clone())
                    .unwrap_or_default();
                window.tab.notice = Some(main_window::another_tab_running(&title));
            }
            self.notice_until = Some(now + NOTICE_SHOWN_FOR);
            return;
        }
        let (hand_over, handed) = mpsc::channel::<HandedConnection>();
        match self.connection.take() {
            Some(connection) => {
                let to_close = std::mem::take(&mut self.sessions_to_close);
                let _ = hand_over.send((connection, to_close));
            }
            None if self.reading.is_some() && self.run_waiting_for_connection.is_none() => {
                self.run_waiting_for_connection = Some(hand_over);
            }
            None => return,
        }
        let (send, answer) = mpsc::channel();
        let cancel = CancelHandle::default();
        let worker_cancel = cancel.clone();
        let statements = request.statements.clone();
        let dialect = request.dialect;
        let session = request.session;
        let scan = request.scan.clone();
        let bracketed = request.explain.as_ref().map(|explain| BracketedRun {
            before: explain.before.clone(),
            statements: statements.clone(),
            after: explain.after.clone(),
            rolled_back: explain.rolled_back,
        });
        std::thread::spawn(move || {
            let Ok((mut connection, to_close)) = handed.recv() else {
                return;
            };
            for closed in to_close {
                connection.close_session(closed);
            }
            let mut page_end = None;
            let outcomes = match (&scan, &bracketed) {
                (Some(scan), _) => vec![connection.scan_keys(scan, &worker_cancel).map(|page| {
                    page_end = Some(page.end);
                    vec![page.table]
                })],
                (None, Some(run)) => {
                    connection.run_bracketed_in_session(session, dialect, run, &worker_cancel)
                }
                (None, None) => {
                    connection.run_in_session(session, dialect, &statements, &worker_cancel)
                }
            };
            let transaction = connection.transaction_status(session);
            let _ = send.send((connection, outcomes, transaction, page_end));
        });
        if let Some(window) = self.main_window_mut() {
            window.tab.started();
        }
        self.running = Some(Running {
            request,
            started: now,
            cancel,
            answer,
        });
    }

    fn finish_run(&mut self, running: Running, answer: RunAnswer, now: Instant) {
        let (connection, outcomes, transaction, page_end) = answer;
        let elapsed = now.saturating_duration_since(running.started);
        let changed_schema = outcomes
            .iter()
            .zip(&running.request.statements)
            .any(|(outcome, statement)| outcome.is_ok() && changes_schema(statement));
        if let Some(tab) = self
            .main_window_mut()
            .and_then(|window| window.tab_with_session_mut(running.request.session))
        {
            if let Some(end) = page_end {
                tab.key_page_read(&running.request, end);
            }
            tab.finish(&running.request, outcomes, elapsed);
            tab.transaction = transaction;
        }
        if let Some(window) = self.main_window_mut() {
            window.refresh_drop_blocked();
        }
        if changed_schema && matches!(self.screen, Screen::Main(_)) {
            self.catalog_wanted = true;
        }
        self.connection_back(connection);
    }

    fn tick_run(&mut self, now: Instant) -> bool {
        let Some(running) = &self.running else {
            return false;
        };
        match running.answer.try_recv() {
            Ok(answer) => {
                if let Some(running) = self.running.take() {
                    self.finish_run(running, answer, now);
                }
                true
            }
            Err(TryRecvError::Disconnected) => {
                let session = running.request.session;
                self.running = None;
                if let Some(tab) = self
                    .main_window_mut()
                    .and_then(|window| window.tab_with_session_mut(session))
                {
                    tab.running = false;
                    tab.stopping = false;
                    tab.summary = "Error".to_owned();
                }
                if let Some(window) = self.main_window_mut() {
                    window.refresh_drop_blocked();
                }
                true
            }
            Err(TryRecvError::Empty) => {
                let elapsed = now.saturating_duration_since(running.started);
                let session = running.request.session;
                self.main_window_mut()
                    .and_then(|window| window.tab_with_session_mut(session))
                    .is_some_and(|tab| tab.show_running(elapsed))
            }
        }
    }

    fn tick_prefix(&mut self, now: Instant) -> bool {
        if self.running.is_some() {
            return false;
        }
        let action = match self.main_window_mut() {
            Some(window) => window.tab.prefix_due(now),
            None => return false,
        };
        let changed = action != TabAction::None;
        self.tab_action(action, now);
        changed
    }

    /// Waits for the running query to end, up to `limit`, and takes its outcome, then for the
    /// catalog read a schema change starts; for scripted runs and tests that render a run's
    /// result.
    pub fn wait_for_run(&mut self, limit: Duration) {
        let until = Instant::now() + limit;
        self.wait_for_reads(limit);
        let Some(running) = self.running.take() else {
            return;
        };
        match running
            .answer
            .recv_timeout(until.saturating_duration_since(Instant::now()))
        {
            Ok(answer) => self.finish_run(running, answer, Instant::now()),
            Err(_) => {
                self.running = Some(running);
                return;
            }
        }
        self.wait_for_reads(until.saturating_duration_since(Instant::now()));
    }

    /// Puts `text` in the editor, the caret at its end, and runs it as `Ctrl/Cmd+Enter`
    /// (or, with `all`, `Ctrl/Cmd+Shift+Enter`) would; for scripted runs.
    pub fn run_text(&mut self, text: &str, all: bool, now: Instant) {
        let Some(window) = self.main_window_mut() else {
            return;
        };
        window.tab.editor = crate::editor::Editor::new(text);
        if let Some(request) = window.tab.run_request(all) {
            self.start_run(request, now);
        }
    }

    /// Puts `text` in the editor, the caret at its end, and explains it as `Ctrl/Cmd+E`
    /// (or, with `analyze`, `Ctrl/Cmd+Shift+E`) would; for scripted runs.
    pub fn explain_text(&mut self, text: &str, analyze: bool, now: Instant) {
        let Some(window) = self.main_window_mut() else {
            return;
        };
        window.tab.editor = crate::editor::Editor::new(text);
        let action = window.tab.explain(analyze);
        self.tab_action(action, now);
    }

    fn act(&mut self, action: ConnectAction, now: Instant) -> WorkspaceCommand {
        match action {
            ConnectAction::Quit => WorkspaceCommand::Quit,
            ConnectAction::Open => {
                self.start_open(now);
                WorkspaceCommand::None
            }
            ConnectAction::Browse => match &self.screen {
                Screen::Connect(card) => WorkspaceCommand::ChooseFile(
                    card.file_dialog_request(self.home.as_deref(), &self.current),
                ),
                Screen::Main(_) => WorkspaceCommand::None,
            },
            ConnectAction::StopOpening => {
                self.opening = None;
                if let Some(card) = self.card_mut() {
                    card.stop_opening();
                }
                self.status_until = Some(now + CANCELLED_SHOWN_FOR);
                WorkspaceCommand::None
            }
            ConnectAction::CopySelection => {
                if let Some(text) = self.card_mut().and_then(|card| card.copied_text()) {
                    self.clipboard.put_text(&text);
                }
                WorkspaceCommand::None
            }
            ConnectAction::Paste => {
                let pasted = self.clipboard.text().map(|raw| pointer::paste_text(&raw));
                let refused = match (pasted, self.card_mut()) {
                    (Some(Ok(text)), Some(card)) => {
                        card.paste(&text);
                        false
                    }
                    (Some(Err(toast)), Some(card)) => {
                        card.status = Some(toast.to_owned());
                        true
                    }
                    _ => false,
                };
                if refused {
                    self.status_until = Some(now + NOTICE_SHOWN_FOR);
                }
                WorkspaceCommand::None
            }
            ConnectAction::ShowLayoutPicker => {
                let layout = self.layout;
                if let Some(card) = self.card_mut() {
                    card.layout_picker = Some(LayoutPicker::opened(layout));
                }
                WorkspaceCommand::None
            }
            ConnectAction::None | ConnectAction::Cancel | ConnectAction::LoadSampleDataset => {
                WorkspaceCommand::None
            }
        }
    }

    /// Puts `typed` in the File input and opens it with the checkboxes as set, as typing
    /// it and pressing `Enter` does; for scripted runs.
    pub fn open_typed(&mut self, typed: &str, now: Instant) {
        if let Some(card) = self.card_mut() {
            card.show_segment(Segment::File);
            card.file.set(typed);
            card.chosen_file = None;
        }
        self.start_open(now);
    }

    /// Takes the open-file dialog's answer: the chosen path, or `None` when it was cancelled.
    pub fn file_chosen(&mut self, chosen: Option<&Path>) {
        if let Some(card) = self.card_mut() {
            card.choose_file(chosen);
        }
    }

    /// A drag carrying `paths` came over the window.
    pub fn drag_entered(&mut self, paths: &[PathBuf]) {
        if let Some(card) = self.card_mut() {
            card.drag_entered(paths);
        }
    }

    /// The drag left the window without a drop.
    pub fn drag_left(&mut self) {
        if let Some(card) = self.card_mut() {
            card.drag_over = false;
        }
    }

    /// Files were dropped onto the window: the first one opens at once.
    pub fn dropped(&mut self, paths: &[PathBuf], now: Instant) {
        let action = match self.card_mut() {
            Some(card) => card.drop_files(paths),
            None => ConnectAction::None,
        };
        if action == ConnectAction::Open {
            self.start_open(now);
        }
    }

    fn start_open(&mut self, now: Instant) {
        let Screen::Connect(card) = &mut self.screen else {
            return;
        };
        let target = match card.segment {
            Segment::File => {
                ConnectTarget::File(card.file_target(self.home.as_deref(), &self.current))
            }
            Segment::Server => match card.server.connect_target() {
                Ok(target) => target,
                Err(failure) => {
                    card.begin_opening();
                    card.finish_with_failure(&failure);
                    return;
                }
            },
        };
        card.begin_opening();
        let (sender, answer) = mpsc::channel();
        let connector = Arc::clone(&self.connector);
        let request = target.clone();
        std::thread::spawn(move || {
            let answer = connector.connect(&request).map(|mut connection| {
                let catalog = opening_catalog(&mut connection);
                (connection, catalog)
            });
            let _ = sender.send(answer);
        });
        self.opening = Some(Opening {
            target,
            started: now,
            answer,
        });
    }

    /// Moves time on to `now`: starts and moves on the 80 ms hover and pressed fades (none
    /// under reduced motion), shows a plan node's tooltip once the pointer has rested on it
    /// for 600 ms, takes a finished open's answer, shows the progress state after
    /// [`PROGRESS_DELAY`] and clears `Cancelled` after [`CANCELLED_SHOWN_FOR`]. Returns
    /// whether the screen changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let mut changed = self.tick_fades(now);
        changed |= self.tick_plan_tooltip(now);
        changed |= self.tick_read();
        changed |= self.tick_run(now);
        changed |= self.tick_prefix(now);
        if self.caret_blinks || !self.window_active {
            changed |= self.refresh_caret(now);
        }
        if self.notice_until.is_some_and(|until| now >= until) {
            self.notice_until = None;
            if let Some(window) = self.main_window_mut() {
                window.tab.notice = None;
            }
            changed = true;
        }
        if self.status_until.is_some_and(|until| now >= until) {
            self.status_until = None;
            if let Some(card) = self.card_mut() {
                card.status = None;
            }
            changed = true;
        }
        let Some(opening) = &self.opening else {
            return changed;
        };
        match opening.answer.try_recv() {
            Ok(answer) => {
                self.finish(answer);
                true
            }
            Err(TryRecvError::Disconnected) => {
                self.opening = None;
                if let Some(card) = self.card_mut() {
                    card.busy = false;
                    card.progress = None;
                }
                true
            }
            Err(TryRecvError::Empty) => {
                let late = now.saturating_duration_since(opening.started) >= PROGRESS_DELAY;
                let target = opening.target.clone();
                match self.card_mut() {
                    Some(card) if late && card.progress.is_none() => {
                        card.show_progress(&target);
                        true
                    }
                    _ => changed,
                }
            }
        }
    }

    /// Waits for the running open to end, up to `limit`, and takes its answer; for scripted
    /// runs that render the outcome of one open.
    pub fn wait_for_open(&mut self, limit: Duration) {
        let Some(opening) = &self.opening else {
            return;
        };
        match opening.answer.recv_timeout(limit) {
            Ok(answer) => self.finish(answer),
            Err(_) => self.opening = None,
        }
    }

    fn finish(&mut self, answer: OpenAnswer) {
        self.opening = None;
        match answer {
            Ok((connection, catalog)) => {
                let mut window = main_window_for(&connection, catalog);
                window.layout = self.layout;
                window.use_sizes(self.saved_sizes(self.layout));
                let first_scan = window.open_on_first_key_space();
                self.screen = Screen::Main(Box::new(window));
                self.connection = Some(connection);
                if let Some(action) = first_scan {
                    self.window_action(action, Instant::now());
                }
            }
            Err(failure) => {
                if let Some(card) = self.card_mut() {
                    card.finish_with_failure(&failure);
                }
            }
        }
    }
}

/// The catalog the main window first shows for `connection`: its full catalog, else its
/// table names (an empty `Keys` group on a RESP server); it reads the database, so it runs
/// off the window's thread.
pub fn opening_catalog(connection: &mut Connection) -> Catalog {
    match connection.catalog() {
        Ok(catalog) => catalog,
        Err(_) if connection.is_resp() => Catalog {
            over_resp: true,
            ..Catalog::default()
        },
        Err(_) => Catalog::of_tables(connection.table_names().unwrap_or_default()),
    }
}

/// The main window as it first shows `connection` (`docs/ux/workspace.md` 1.7): its name and
/// location in the toolbar, `catalog` in the object tree, one empty `Query 1` editor (a RESP
/// server's first key space replaces it once the window shows, [`MainWindow::open_on_first_key_space`]).
pub fn main_window_for(connection: &Connection, catalog: Catalog) -> MainWindow {
    MainWindow::with_catalog(
        connection.name(),
        connection.location(),
        !connection.read_only(),
        catalog,
    )
}

/// Whether `statement` may change what the object tree lists: a `CREATE`, `ALTER`, `DROP`
/// or `RENAME`, or SQL Server's `sp_rename`.
pub fn changes_schema(statement: &str) -> bool {
    let first: String = statement
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    match first.to_ascii_uppercase().as_str() {
        "CREATE" | "ALTER" | "DROP" | "RENAME" => true,
        "EXEC" | "EXECUTE" | "SP_RENAME" => statement.to_ascii_lowercase().contains("sp_rename"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect_screen::{Focus, Segment};
    use crate::connection::{EngineConnector, FailureField};
    use eggdb_platform::window::Key;

    #[derive(Debug)]
    struct SlowConnector(Duration);

    impl Connector for SlowConnector {
        fn connect(&self, target: &ConnectTarget) -> Result<Connection, ConnectFailure> {
            std::thread::sleep(self.0);
            match target {
                ConnectTarget::File(file) => Connection::in_memory(
                    &file.path.file_name().unwrap_or_default().to_string_lossy(),
                ),
                ConnectTarget::Server(_) | ConnectTarget::Resp(_) => {
                    Err(crate::connection::url_failure())
                }
            }
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("eggdb-gui-workspace-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&directory);
        directory
    }

    fn workspace(connector: Arc<dyn Connector>, current: PathBuf) -> Workspace {
        Workspace::new(
            connector,
            Some(PathBuf::from("/home/nobody")),
            current,
            true,
        )
    }

    fn type_text(workspace: &mut Workspace, text: &str, now: Instant) {
        for character in text.chars() {
            workspace.key(KeyPress::plain(Key::Character(character)), now);
        }
    }

    fn card(workspace: &mut Workspace) -> ConnectCard {
        workspace.card_mut().cloned().expect("the card is shown")
    }

    fn card_tree(workspace: &Workspace) -> Vec<AccessNode> {
        let Screen::Connect(card) = workspace.screen() else {
            panic!("the card is shown");
        };
        let faces = crate::main_window::Faces {
            ui: &crate::font::BoxFace,
            ui_bold: &crate::font::BoxFace,
            mono: &crate::font::BoxFace,
        };
        crate::connect_screen::accessibility_tree(
            card,
            faces,
            (
                crate::connect_screen::DEFAULT_WIDTH,
                crate::connect_screen::DEFAULT_HEIGHT,
            ),
        )
    }

    fn request(node: u32, action: eggdb_platform::window::AccessAction) -> AccessRequest {
        AccessRequest { node, action }
    }

    #[test]
    fn screen_reader_requests_act_on_the_card_like_the_keys_do() {
        use crate::connect_screen::{
            NODE_CONNECT_TO_A_SERVER, NODE_FILE, NODE_OPEN, NODE_OPEN_A_FILE, NODE_READ_ONLY,
            NODE_TITLE,
        };
        use eggdb_platform::window::AccessAction;
        let directory = scratch("access");
        let path = directory.join("spoken.eggdb");
        let _ = std::fs::remove_file(&path);
        let now = Instant::now();
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        let tree = card_tree(&workspace);
        workspace.access(&request(NODE_READ_ONLY, AccessAction::Toggle), &tree, now);
        assert!(card(&mut workspace).read_only);
        assert_eq!(card(&mut workspace).focus, Focus::ReadOnly);
        let tree = card_tree(&workspace);
        assert_eq!(
            workspace.access(&request(NODE_READ_ONLY, AccessAction::Invoke), &tree, now),
            WorkspaceCommand::None
        );
        assert!(
            card(&mut workspace).read_only,
            "a checkbox offers toggle, not invoke"
        );
        let tree = card_tree(&workspace);
        workspace.access(&request(NODE_READ_ONLY, AccessAction::Toggle), &tree, now);
        assert!(
            !card(&mut workspace).read_only,
            "toggling a checkbox again flips it back"
        );
        let tree = card_tree(&workspace);
        assert_eq!(
            workspace.access(&request(NODE_TITLE, AccessAction::Focus), &tree, now),
            WorkspaceCommand::None
        );
        assert_eq!(
            card(&mut workspace).focus,
            Focus::ReadOnly,
            "a heading takes no focus"
        );
        workspace.access(
            &request(NODE_FILE, AccessAction::SetValue("spoken.eggdb".to_owned())),
            &tree,
            now,
        );
        let typed = card(&mut workspace);
        assert_eq!(
            (typed.focus, typed.file.value()),
            (Focus::FileInput, "spoken.eggdb")
        );
        let tree = card_tree(&workspace);
        let caret = eggdb_platform::window::AccessCaret {
            anchor: 0,
            caret: 7,
        };
        workspace.access(
            &request(NODE_FILE, AccessAction::SetCaret(caret)),
            &tree,
            now,
        );
        assert_eq!(card(&mut workspace).file.selection(), Some((0, 7)));
        let tree = card_tree(&workspace);
        workspace.access(
            &request(NODE_CONNECT_TO_A_SERVER, AccessAction::Select),
            &tree,
            now,
        );
        assert_eq!(card(&mut workspace).segment, Segment::Server);
        workspace.access(
            &request(NODE_OPEN_A_FILE, AccessAction::Select),
            &card_tree(&workspace),
            now,
        );
        let tree = card_tree(&workspace);
        workspace.access(
            &request(NODE_FILE, AccessAction::SetValue("spoken.eggdb".to_owned())),
            &tree,
            now,
        );
        let tree = card_tree(&workspace);
        workspace.access(&request(NODE_OPEN, AccessAction::Invoke), &tree, now);
        workspace.wait_for_open(Duration::from_secs(10));
        assert!(
            matches!(workspace.screen(), Screen::Main(_)),
            "invoking Open opens the file"
        );
    }

    #[test]
    fn a_typed_relative_path_and_enter_open_the_main_window_on_it() {
        let directory = scratch("typed");
        let path = directory.join("shop.eggdb");
        let _ = std::fs::remove_file(&path);
        let now = Instant::now();
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        type_text(&mut workspace, "  shop.eggdb ", now);
        assert_eq!(
            workspace.key(KeyPress::plain(Key::Enter), now),
            WorkspaceCommand::None
        );
        assert!(card(&mut workspace).busy);
        workspace.wait_for_open(Duration::from_secs(10));
        let Screen::Main(window) = workspace.screen() else {
            panic!("the main window replaces the card");
        };
        assert_eq!(window.database, "shop");
        assert_eq!(window.location, "shop.eggdb");
        assert!(window.writable && window.tab.editor.text().is_empty());
        let connection = workspace.connection_mut().unwrap();
        assert_eq!(
            connection.status_text(),
            "Connected · shop.eggdb · read-write"
        );
        assert!(path.exists(), "Create was checked at first start");
        drop(workspace);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn each_tab_keeps_its_own_session_so_a_begin_outlasts_its_run() {
        let directory = scratch("kept-sessions");
        let _ = std::fs::remove_file(directory.join("kept.eggdb"));
        let now = Instant::now();
        let limit = Duration::from_secs(10);
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        type_text(&mut workspace, "kept.eggdb", now);
        workspace.key(KeyPress::plain(Key::Enter), now);
        workspace.wait_for_open(limit);
        workspace.run_text("CREATE TABLE t (id integer)", false, now);
        workspace.wait_for_run(limit);
        workspace.run_text("BEGIN; INSERT INTO t VALUES (1);", true, now);
        workspace.wait_for_run(limit);
        let first_cell = |workspace: &mut Workspace| {
            workspace
                .main_window_mut()
                .and_then(|window| window.tab.grid.as_ref())
                .and_then(|grid| grid.rows.first())
                .and_then(|row| row.first())
                .cloned()
                .flatten()
        };
        let window = workspace.main_window_mut().unwrap();
        assert_eq!(window.tab.transaction, TransactionStatus::InBlock);
        window.add_tab();
        assert_eq!(window.tab.title, "Query 2");
        assert_eq!(window.tab_titles(), ["Query 1", "Query 2"]);
        workspace.run_text("SELECT count(*) FROM t", false, now);
        workspace.wait_for_run(limit);
        assert_eq!(first_cell(&mut workspace).as_deref(), Some("0"));
        let window = workspace.main_window_mut().unwrap();
        assert_eq!(window.tab.transaction, TransactionStatus::Idle);
        assert_eq!(
            window
                .other_tab_in_transaction()
                .map(|tab| tab.title.as_str()),
            Some("Query 1")
        );
        window.show_tab(0);
        workspace.run_text("SELECT count(*) FROM t", false, now);
        workspace.wait_for_run(limit);
        assert_eq!(first_cell(&mut workspace).as_deref(), Some("1"));
        workspace.run_text("COMMIT", false, now);
        workspace.wait_for_run(limit);
        let window = workspace.main_window_mut().unwrap();
        assert_eq!(window.tab.transaction, TransactionStatus::Idle);
        window.show_tab(1);
        workspace.run_text("SELECT count(*) FROM t", false, now);
        workspace.wait_for_run(limit);
        assert_eq!(first_cell(&mut workspace).as_deref(), Some("1"));
    }

    #[test]
    fn closing_a_tab_rolls_back_its_open_transaction() {
        let directory = scratch("kept-close");
        let _ = std::fs::remove_file(directory.join("closed.eggdb"));
        let now = Instant::now();
        let limit = Duration::from_secs(10);
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        type_text(&mut workspace, "closed.eggdb", now);
        workspace.key(KeyPress::plain(Key::Enter), now);
        workspace.wait_for_open(limit);
        workspace.run_text("CREATE TABLE t (id integer)", false, now);
        workspace.wait_for_run(limit);
        workspace
            .main_window_mut()
            .expect("the main window")
            .add_tab();
        workspace.run_text("BEGIN; INSERT INTO t VALUES (1);", true, now);
        workspace.wait_for_run(limit);
        let close = KeyPress {
            key: Key::Character('w'),
            modifiers: Modifiers {
                control: true,
                ..Modifiers::default()
            },
        };
        workspace.key(close, now);
        let window = workspace.main_window_mut().unwrap();
        let prompt = window.close_prompt.clone().expect("closing asks first");
        assert_eq!(prompt.title, "Close Query 2 and roll back?");
        workspace.key(KeyPress::plain(Key::Enter), now);
        let window = workspace.main_window_mut().unwrap();
        assert_eq!(window.tab_titles(), ["Query 1"]);
        assert!(window.close_prompt.is_none());
        workspace.wait_for_reads(limit);
        let connection = workspace.connection_mut().expect("the connection is back");
        let tables = connection.run("SELECT count(*) FROM t").unwrap();
        assert_eq!(
            tables.last().and_then(|table| table.rows.first()).cloned(),
            Some(vec![Some("0".to_owned())])
        );
    }

    #[test]
    fn drop_from_the_tree_menu_runs_in_its_own_session_and_refreshes_the_tree() {
        let directory = scratch("tree-drop");
        let _ = std::fs::remove_file(directory.join("dropped.eggdb"));
        let now = Instant::now();
        let limit = Duration::from_secs(10);
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        type_text(&mut workspace, "dropped.eggdb", now);
        workspace.key(KeyPress::plain(Key::Enter), now);
        workspace.wait_for_open(limit);
        workspace.run_text("CREATE TABLE gone (id integer)", false, now);
        workspace.wait_for_run(limit);
        workspace.refresh_catalog();
        workspace.wait_for_reads(limit);
        let window = workspace.main_window_mut().expect("the main window");
        let table = window
            .tree
            .catalog
            .tables
            .iter()
            .position(|table| table.name == "gone")
            .expect("the table is listed");
        window.focus_tree();
        window.tree.current = TreeItem::Table(table);
        let menu_key = KeyPress {
            key: Key::F10,
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        };
        workspace.key(menu_key, now);
        let window = workspace.main_window_mut().unwrap();
        let menu = window.tree_menu.expect("Shift+F10 opens the row menu");
        assert_eq!(menu.row, TreeItem::Table(table));
        workspace.key(KeyPress::plain(Key::End), now);
        workspace.key(KeyPress::plain(Key::Enter), now);
        let window = workspace.main_window_mut().unwrap();
        let (prompt, _) = window.drop_prompt.clone().expect("Drop… opens the dialog");
        assert_eq!(prompt.statement, "DROP TABLE gone;");
        workspace.key(KeyPress::plain(Key::Tab), now);
        workspace.key(KeyPress::plain(Key::Tab), now);
        workspace.key(KeyPress::plain(Key::Enter), now);
        workspace.wait_for_reads(limit);
        let window = workspace.main_window_mut().unwrap();
        assert!(window.drop_prompt.is_none());
        assert_eq!(window.tab.notice.as_deref(), Some("Dropped table gone"));
        assert!(
            !window
                .tree
                .catalog
                .tables
                .iter()
                .any(|table| table.name == "gone")
        );
        assert_eq!(
            window.tree.selected,
            Some(TreeItem::Group(crate::object_browser::Group::Tables))
        );
    }

    #[test]
    fn a_run_started_while_the_catalog_is_read_waits_for_the_connection_and_runs() {
        let directory = scratch("read-then-run");
        let now = Instant::now();
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        type_text(&mut workspace, "reads.eggdb", now);
        workspace.key(KeyPress::plain(Key::Enter), now);
        workspace.wait_for_open(Duration::from_secs(10));
        workspace.refresh_catalog();
        assert!(workspace.connection_mut().is_none(), "the read holds it");
        workspace.run_text("CREATE TABLE late (id integer)", false, now);
        assert!(workspace.is_running());
        workspace.wait_for_run(Duration::from_secs(10));
        let Screen::Main(window) = workspace.screen() else {
            panic!("the main window stays");
        };
        assert_eq!(window.tab.messages, ["CREATE TABLE"]);
        assert!(
            window
                .tree
                .catalog
                .tables
                .iter()
                .any(|table| table.name == "late"),
            "the schema change read the catalog again"
        );
        assert!(workspace.connection_mut().is_some());
    }

    #[test]
    fn a_missing_file_with_create_unchecked_shows_the_error_lines_and_selects_the_input() {
        let directory = scratch("missing");
        let path = directory.join("absent.eggdb");
        let _ = std::fs::remove_file(&path);
        let now = Instant::now();
        let mut workspace = workspace(Arc::new(EngineConnector), directory);
        type_text(&mut workspace, &path.display().to_string(), now);
        workspace.key(KeyPress::plain(Key::Tab), now);
        workspace.key(KeyPress::plain(Key::Tab), now);
        assert_eq!(card(&mut workspace).focus, Focus::CreateIfMissing);
        workspace.key(KeyPress::plain(Key::Space), now);
        workspace.key(KeyPress::plain(Key::Enter), now);
        workspace.wait_for_open(Duration::from_secs(10));
        let shown = card(&mut workspace);
        let error = shown.error.clone().expect("the error lines are shown");
        let path_text = path.display().to_string();
        assert_eq!(
            error.line,
            format!("Error: cannot open {path_text}: no such file")
        );
        assert_eq!(
            error.fix,
            Some(format!(
                "Try: check \"Create the file if it does not exist\", or eggdb create {path_text}"
            ))
        );
        assert_eq!(shown.focus, Focus::FileInput);
        assert_eq!(shown.file.selection(), Some((0, path_text.chars().count())));
        assert!(!shown.busy && !path.exists());
        workspace.key(KeyPress::plain(Key::Enter), now);
        assert_eq!(
            card(&mut workspace).error,
            None,
            "a second attempt clears the lines"
        );
    }

    #[test]
    fn a_slow_open_shows_progress_after_120_ms_and_escape_cancels_it() {
        let now = Instant::now();
        let mut workspace = workspace(
            Arc::new(SlowConnector(Duration::from_secs(1))),
            scratch("slow"),
        );
        workspace.key(KeyPress::plain(Key::Enter), now);
        assert!(!workspace.tick(now + Duration::from_millis(50)));
        assert_eq!(card(&mut workspace).progress, None);
        assert!(workspace.tick(now + Duration::from_millis(200)));
        let shown = card(&mut workspace);
        assert_eq!(shown.progress.as_deref(), Some("Opening quickstart.eggdb…"));
        assert!(shown.disabled());
        let stopped = now + Duration::from_millis(500);
        workspace.key(KeyPress::plain(Key::Character('x')), stopped);
        assert_eq!(
            card(&mut workspace).file.value(),
            "~/eggdb/quickstart.eggdb"
        );
        workspace.key(KeyPress::plain(Key::Escape), stopped);
        let shown = card(&mut workspace);
        assert!(!shown.busy && !shown.disabled() && !workspace.is_opening());
        assert_eq!(shown.status.as_deref(), Some("Cancelled"));
        assert_eq!(shown.focus, Focus::FileInput);
        assert!(workspace.tick(stopped + CANCELLED_SHOWN_FOR));
        assert_eq!(card(&mut workspace).status, None);
    }

    #[test]
    fn browse_asks_for_eggdb_files_and_its_answer_fills_the_input() {
        let directory = scratch("browse");
        let now = Instant::now();
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        workspace.key(KeyPress::plain(Key::Tab), now);
        let WorkspaceCommand::ChooseFile(dialog) = workspace.key(KeyPress::plain(Key::Enter), now)
        else {
            panic!("Browse… asks for the dialog");
        };
        assert_eq!(dialog.filters[0].patterns, vec!["*.eggdb".to_owned()]);
        assert_eq!(dialog.filters[1].name, "All files");
        assert_eq!(dialog.start_in, Some(PathBuf::from("/home/nobody")));
        workspace.file_chosen(None);
        assert_eq!(card(&mut workspace).focus, Focus::Browse);
        let chosen = directory.join("chosen.eggdb");
        workspace.file_chosen(Some(&chosen));
        let shown = card(&mut workspace);
        assert_eq!(shown.file.value(), chosen.display().to_string());
        assert_eq!(shown.file.selection(), None);
        assert_eq!(shown.focus, Focus::FileInput);
        let mut card = shown;
        card.file_dialog = false;
        assert!(!card.focus_order().contains(&Focus::Browse));
    }

    #[test]
    fn dropping_files_opens_the_first_and_ignores_directories() {
        let directory = scratch("drop");
        let first = directory.join("dropped.eggdb");
        let _ = std::fs::remove_file(&first);
        drop(
            eggdb::Database::open_with(&first, eggdb::Options::new().create_if_missing(true))
                .expect("the dropped file is made first"),
        );
        let now = Instant::now();
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        workspace.drag_entered(std::slice::from_ref(&directory));
        assert!(!card(&mut workspace).drag_over, "a directory shows no ring");
        workspace.dropped(std::slice::from_ref(&directory), now);
        assert!(!workspace.is_opening());
        workspace.drag_entered(std::slice::from_ref(&first));
        assert!(card(&mut workspace).drag_over);
        workspace.drag_left();
        assert!(!card(&mut workspace).drag_over);
        let others = [
            first.clone(),
            directory.join("b.eggdb"),
            directory.join("c.eggdb"),
        ];
        workspace.dropped(&others, now);
        assert_eq!(
            card(&mut workspace).status.as_deref(),
            Some("Opened the first of 3 files")
        );
        workspace.wait_for_open(Duration::from_secs(10));
        let Screen::Main(window) = workspace.screen() else {
            panic!("the drop opens the file");
        };
        assert_eq!(window.location, "dropped.eggdb");
        drop(workspace);
        let _ = std::fs::remove_file(&first);
    }

    #[test]
    fn a_failure_about_the_file_points_at_the_file_input() {
        let mut card = ConnectCard::first_start();
        card.focus = Focus::Open;
        card.begin_opening();
        card.finish_with_failure(&ConnectFailure {
            code: Some("28P01".to_owned()),
            message: "password authentication failed for user \"ada\"".to_owned(),
            fix: None,
            field: FailureField::File,
        });
        let error = card.error.clone().unwrap();
        assert_eq!(
            error.line,
            "Error 28P01: password authentication failed for user \"ada\""
        );
        assert_eq!(error.fix, None);
        assert_eq!(card.focus, Focus::FileInput);
    }

    #[test]
    fn a_slow_connect_shows_connecting_to_the_host_and_port() {
        let mut workspace = workspace(
            Arc::new(SlowConnector(Duration::from_millis(400))),
            scratch("server"),
        );
        let start = Instant::now();
        if let Some(card) = workspace.card_mut() {
            card.show_segment(Segment::Server);
            card.focus = Focus::Url;
        }
        type_text(&mut workspace, "eggdb://ada@db.example.com/shop", start);
        workspace.key(KeyPress::plain(Key::Enter), start);
        assert!(workspace.is_opening());
        workspace.tick(start + PROGRESS_DELAY);
        let shown = card(&mut workspace);
        assert_eq!(
            shown.progress.as_deref(),
            Some("Connecting to db.example.com:5432…")
        );
        assert!(shown.disabled());
        workspace.wait_for_open(Duration::from_secs(5));
        let failed = card(&mut workspace);
        assert_eq!(failed.focus, Focus::Url);
        assert_eq!(
            failed.error.map(|error| error.line).as_deref(),
            Some("Error: the URL is not a database URL")
        );
    }

    #[test]
    fn typing_a_path_after_the_server_segment_opens_the_file() {
        let directory = scratch("back-to-file");
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        if let Some(card) = workspace.card_mut() {
            card.show_segment(Segment::Server);
        }
        workspace.open_typed("back.eggdb", Instant::now());
        workspace.wait_for_open(Duration::from_secs(10));
        assert!(matches!(workspace.screen(), Screen::Main(_)));
        let _ = std::fs::remove_file(directory.join("back.eggdb"));
    }

    const POINTER_FACES: crate::main_window::Faces<'static> = crate::main_window::Faces {
        ui: &crate::font::BoxFace,
        ui_bold: &crate::font::BoxFace,
        mono: &crate::font::BoxFace,
    };
    const WINDOW: (u32, u32) = (1280, 800);

    fn bounds_of(workspace: &Workspace, id: u32) -> eggdb_platform::window::AccessRect {
        workspace
            .pointer_tree(POINTER_FACES, WINDOW)
            .into_iter()
            .find(|node| node.id == id)
            .map(|node| node.bounds)
            .expect("the node is in the tree")
    }

    fn centre_of(workspace: &Workspace, id: u32) -> (f32, f32) {
        let bounds = bounds_of(workspace, id);
        (
            bounds.x + bounds.width / 2.0,
            bounds.y + bounds.height / 2.0,
        )
    }

    fn click(workspace: &mut Workspace, (x, y): (f32, f32), now: Instant) -> WorkspaceCommand {
        let _ = workspace.pointer_pressed(
            x,
            y,
            PointerButton::Primary,
            Modifiers::default(),
            POINTER_FACES,
            WINDOW,
            now,
        );
        workspace.pointer_released(x, y, PointerButton::Primary, POINTER_FACES, WINDOW, now)
    }

    #[test]
    fn a_checkbox_toggles_on_release_inside_and_a_release_outside_cancels() {
        let mut workspace = workspace(
            Arc::new(SlowConnector(Duration::ZERO)),
            scratch("pointer-check"),
        );
        let now = Instant::now();
        let at = centre_of(&workspace, connect_screen::NODE_READ_ONLY);
        assert!(workspace.pointer_moved(at.0, at.1, POINTER_FACES, WINDOW));
        assert!(
            card(&mut workspace)
                .pointer
                .is_hovered(connect_screen::NODE_READ_ONLY)
        );
        let _ = workspace.pointer_pressed(
            at.0,
            at.1,
            PointerButton::Primary,
            Modifiers::default(),
            POINTER_FACES,
            WINDOW,
            now,
        );
        assert!(
            card(&mut workspace)
                .pointer
                .is_pressed(connect_screen::NODE_READ_ONLY)
        );
        workspace.pointer_moved(2.0, 2.0, POINTER_FACES, WINDOW);
        assert!(
            !card(&mut workspace)
                .pointer
                .is_pressed(connect_screen::NODE_READ_ONLY)
        );
        workspace.pointer_released(2.0, 2.0, PointerButton::Primary, POINTER_FACES, WINDOW, now);
        assert!(
            !card(&mut workspace).read_only,
            "a release outside changes nothing"
        );
        assert_eq!(card(&mut workspace).pointer.pressed, None);
        click(&mut workspace, at, now + Duration::from_secs(1));
        let shown = card(&mut workspace);
        assert!(shown.read_only);
        assert_eq!(shown.focus, Focus::ReadOnly);
        assert!(
            shown.pointer.focus_by_pointer && !shown.ring_shown(),
            "a click draws no ring on a checkbox"
        );
        workspace.key(KeyPress::plain(Key::Space), now);
        assert!(
            card(&mut workspace).ring_shown(),
            "a key brings the ring back"
        );
        assert!(workspace.pointer_left());
        assert_eq!(card(&mut workspace).pointer.hovered, None);
    }

    #[test]
    fn a_click_and_a_drag_in_the_file_input_place_the_caret_and_select_and_paste_replaces_it() {
        let mut workspace = workspace(
            Arc::new(SlowConnector(Duration::ZERO)),
            scratch("pointer-field"),
        );
        let now = Instant::now();
        workspace
            .card_mut()
            .expect("the card")
            .file
            .set("shop.eggdb");
        workspace.card_mut().expect("the card").focus = Focus::Open;
        let input = bounds_of(&workspace, connect_screen::NODE_FILE);
        let (right, left, middle) = (
            input.x + input.width - 2.0,
            input.x + 1.0,
            input.y + input.height / 2.0,
        );
        let _ = workspace.pointer_pressed(
            right,
            middle,
            PointerButton::Primary,
            Modifiers::default(),
            POINTER_FACES,
            WINDOW,
            now,
        );
        let shown = card(&mut workspace);
        assert_eq!(shown.focus, Focus::FileInput);
        assert_eq!((shown.file.caret(), shown.file.selection()), (10, None));
        assert!(shown.ring_shown(), "inputs keep the ring after a click");
        assert!(workspace.pointer_moved(left, middle, POINTER_FACES, WINDOW));
        workspace.pointer_released(
            left,
            middle,
            PointerButton::Primary,
            POINTER_FACES,
            WINDOW,
            now,
        );
        assert_eq!(card(&mut workspace).file.selection(), Some((0, 10)));
        workspace.key(
            KeyPress {
                key: Key::Character('c'),
                modifiers: crate::accessibility::primary(),
            },
            now,
        );
        assert_eq!(workspace.clipboard().text().as_deref(), Some("shop.eggdb"));
        let mut clipboard = MemoryClipboard::default();
        clipboard.put_text("/tmp/other.eggdb\r\n");
        workspace.set_clipboard(Box::new(clipboard));
        workspace.key(
            KeyPress {
                key: Key::Character('v'),
                modifiers: crate::accessibility::primary(),
            },
            now,
        );
        assert_eq!(card(&mut workspace).file.value(), "/tmp/other.eggdb");
        let mut large = MemoryClipboard::default();
        large.put_text(&"x".repeat(pointer::MAX_PASTE_BYTES + 1));
        workspace.set_clipboard(Box::new(large));
        workspace.key(
            KeyPress {
                key: Key::Character('v'),
                modifiers: crate::accessibility::primary(),
            },
            now,
        );
        let shown = card(&mut workspace);
        assert_eq!(shown.file.value(), "/tmp/other.eggdb");
        assert_eq!(shown.status.as_deref(), Some(pointer::PASTE_TOO_LARGE));
    }

    #[test]
    fn clicking_open_opens_and_a_click_in_the_editor_places_the_caret() {
        use crate::font::Typeface as _;
        let directory = scratch("pointer-open");
        let mut workspace = workspace(Arc::new(SlowConnector(Duration::ZERO)), directory.clone());
        let now = Instant::now();
        workspace
            .card_mut()
            .expect("the card")
            .file
            .set(&directory.join("clicked.eggdb").display().to_string());
        let open = centre_of(&workspace, connect_screen::NODE_OPEN);
        click(&mut workspace, open, now);
        workspace.wait_for_open(Duration::from_secs(10));
        assert!(
            matches!(workspace.screen(), Screen::Main(_)),
            "a click on Open opens"
        );
        workspace
            .main_window_mut()
            .expect("the main window")
            .tab
            .editor = crate::editor::Editor::new("SELECT 1;\nSELECT 22;");
        let editor = bounds_of(&workspace, main_window::NODE_EDITOR);
        let advance = crate::font::BoxFace.advance('x', 13.0);
        let at = (320.0 + advance * 3.0 + 1.0, 88.0 + 20.0 + 5.0);
        assert!(pointer::contains(editor, at.0, at.1));
        click(&mut workspace, at, now + Duration::from_secs(2));
        let window = workspace.main_window_mut().expect("the main window");
        assert_eq!(
            window.tab.editor.caret(),
            crate::editor::Position { line: 1, column: 3 }
        );
        assert!(window.pointer.focus_by_pointer);
        let _ = workspace.pointer_pressed(
            at.0,
            at.1,
            PointerButton::Primary,
            Modifiers::default(),
            POINTER_FACES,
            WINDOW,
            now + Duration::from_millis(2_100),
        );
        let window = workspace.main_window_mut().expect("the main window");
        assert_eq!(
            window.tab.editor.selected_text().as_deref(),
            Some("SELECT"),
            "a double-click selects the word"
        );
        let _ = std::fs::remove_file(directory.join("clicked.eggdb"));
    }

    #[test]
    fn the_caret_blinks_only_once_a_window_starts_it_and_goes_grey_when_inactive() {
        let mut workspace = workspace(
            Arc::new(SlowConnector(Duration::ZERO)),
            scratch("pointer-caret"),
        );
        let start = Instant::now();
        assert!(!workspace.tick(start + Duration::from_millis(1_100)));
        assert_eq!(card(&mut workspace).caret, pointer::CaretLook::On);
        assert_eq!(workspace.until_caret_changes(start), None);
        workspace.start_caret_blink(start);
        assert!(workspace.tick(start + Duration::from_millis(1_100)));
        assert_eq!(card(&mut workspace).caret, pointer::CaretLook::Off);
        workspace.key(
            KeyPress::plain(Key::End),
            start + Duration::from_millis(1_200),
        );
        assert_eq!(card(&mut workspace).caret, pointer::CaretLook::On);
        assert_eq!(
            workspace.until_caret_changes(start + Duration::from_millis(1_600)),
            Some(Duration::from_millis(630))
        );
        assert!(workspace.set_window_active(false, start + Duration::from_millis(1_700)));
        assert_eq!(card(&mut workspace).caret, pointer::CaretLook::Inactive);
        workspace.set_window_active(true, start + Duration::from_millis(1_800));
        workspace.set_reduced_motion(true);
        workspace.tick(start + Duration::from_millis(2_800));
        assert_eq!(card(&mut workspace).caret, pointer::CaretLook::On);
    }

    fn with_url_password(workspace: &mut Workspace) {
        let card = workspace.card_mut().expect("the card");
        card.show_segment(Segment::Server);
        card.move_focus(Focus::Url);
        card.server.url.set("postgresql://ada:hunter2@db/shop");
        card.server.url_edited();
    }

    #[test]
    fn leaving_the_url_input_by_click_screen_reader_or_segment_forgets_its_password() {
        let mut workspace = workspace(
            Arc::new(SlowConnector(Duration::ZERO)),
            scratch("url-password"),
        );
        let now = Instant::now();
        with_url_password(&mut workspace);
        let host = centre_of(&workspace, connect_screen::NODE_HOST);
        let _ = click(&mut workspace, host, now);
        let shown = card(&mut workspace);
        assert_eq!(shown.focus, Focus::Host);
        assert_eq!(shown.server.url.value(), "postgresql://ada@db:5432/shop");
        assert_eq!(shown.server.password.value(), "hunter2");
        with_url_password(&mut workspace);
        let tree = workspace.pointer_tree(POINTER_FACES, WINDOW);
        let _ = workspace.access(
            &request(connect_screen::NODE_DATABASE, AccessAction::Focus),
            &tree,
            now,
        );
        let shown = card(&mut workspace);
        assert_eq!(shown.focus, Focus::Database);
        assert_eq!(shown.server.url.value(), "postgresql://ada@db:5432/shop");
        with_url_password(&mut workspace);
        workspace
            .card_mut()
            .expect("the card")
            .show_segment(Segment::File);
        assert!(!card(&mut workspace).server.url.value().contains("hunter2"));
    }

    #[test]
    fn a_screen_reader_sets_a_card_field_in_one_edit() {
        let mut workspace = workspace(
            Arc::new(SlowConnector(Duration::ZERO)),
            scratch("set-value"),
        );
        let now = Instant::now();
        if let Some(card) = workspace.card_mut() {
            card.show_segment(Segment::Server);
        }
        let tree = workspace.pointer_tree(POINTER_FACES, WINDOW);
        let spoken = "postgresql://ada@db:5433/shop\u{7}".to_owned();
        let _ = workspace.access(
            &request(connect_screen::NODE_URL, AccessAction::SetValue(spoken)),
            &tree,
            now,
        );
        let shown = card(&mut workspace);
        assert_eq!(shown.server.url.value(), "postgresql://ada@db:5433/shop");
        assert_eq!(
            (shown.server.host.value(), shown.server.port.value()),
            ("db", "5433")
        );
        assert_eq!(shown.server.url.caret(), shown.server.url.value().len());
    }

    #[test]
    fn a_dropped_path_opens_exactly_as_given_and_is_never_created() {
        let directory = scratch("dropped");
        let missing = directory.join("missing.eggdb ");
        let _ = std::fs::remove_file(&missing);
        let now = Instant::now();
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        assert!(card(&mut workspace).create_if_missing);
        workspace.dropped(std::slice::from_ref(&missing), now);
        workspace.wait_for_open(Duration::from_secs(10));
        let shown = card(&mut workspace);
        let error = shown.error.expect("a missing dropped file fails");
        assert!(
            error.line.ends_with("missing.eggdb : no such file"),
            "{}",
            error.line
        );
        assert!(!missing.exists());
        assert!(!directory.join("missing.eggdb").exists());
    }

    #[derive(Debug)]
    struct TwoTables;

    impl Connector for TwoTables {
        fn connect(&self, _: &ConnectTarget) -> Result<Connection, ConnectFailure> {
            let mut connection = Connection::in_memory("two.eggdb")?;
            connection
                .run("CREATE TABLE alpha (x INT); CREATE TABLE beta (x INT)")
                .map_err(|failure| ConnectFailure {
                    code: None,
                    message: failure.message,
                    fix: None,
                    field: FailureField::File,
                })?;
            Ok(connection)
        }
    }

    #[test]
    fn returning_to_the_table_being_sampled_drops_the_one_wanted_next() {
        let mut workspace = workspace(Arc::new(TwoTables), scratch("samples"));
        workspace.open_typed("two.eggdb", Instant::now());
        workspace.wait_for_open(Duration::from_secs(10));
        let window = workspace.main_window_mut().expect("the main window");
        let names: Vec<_> = (0..2)
            .map(|table| inspector::sampled_name(&window.tree, TreeItem::Table(table)))
            .collect();
        assert!(names.iter().all(Option::is_some), "{names:?}");
        assert!(workspace.reading.is_none());
        workspace.inspect(TreeItem::Table(0));
        let first = CatalogRead::Sample(names[0].clone().unwrap_or_default());
        assert!(
            workspace
                .reading
                .as_ref()
                .is_some_and(|reading| reading.read == first)
        );
        workspace.inspect(TreeItem::Table(1));
        assert_eq!(workspace.sample_wanted, names[1]);
        workspace.inspect(TreeItem::Table(0));
        assert_eq!(workspace.sample_wanted, None);
    }

    #[test]
    fn a_closed_tab_s_session_closes_on_a_worker_and_its_transaction_rolls_back() {
        let mut workspace = workspace(Arc::new(EngineConnector), scratch("close-session"));
        workspace.open_typed("closing.eggdb", Instant::now());
        workspace.wait_for_open(Duration::from_secs(30));
        workspace.wait_for_reads(Duration::from_secs(10));
        let now = Instant::now();
        workspace.run_text("BEGIN", false, now);
        workspace.wait_for_run(Duration::from_secs(10));
        let session = workspace
            .main_window_mut()
            .expect("the main window")
            .tab
            .session;
        assert_eq!(
            workspace
                .connection
                .as_ref()
                .map(|connection| connection.transaction_status(session)),
            Some(TransactionStatus::InBlock)
        );
        workspace.window_action(
            WindowAction::CloseSession {
                session,
                stop: false,
            },
            now,
        );
        assert!(
            workspace.connection.is_none(),
            "the window's thread hands the connection to a worker"
        );
        assert!(
            workspace
                .reading
                .as_ref()
                .is_some_and(|reading| reading.read == CatalogRead::CloseSessions(vec![session]))
        );
        workspace.wait_for_reads(Duration::from_secs(10));
        assert!(workspace.sessions_to_close.is_empty());
        assert_eq!(
            workspace
                .connection
                .as_ref()
                .map(|connection| connection.transaction_status(session)),
            Some(TransactionStatus::Idle)
        );
    }

    #[test]
    fn the_card_s_layout_picker_keeps_the_choice_and_the_main_window_opens_in_it() {
        let directory = scratch("layout-picker");
        let settings = directory.join("gui-settings");
        std::fs::write(&settings, "theme = dark\n").expect("settings are written");
        let mut workspace = workspace(Arc::new(EngineConnector), directory.clone());
        workspace.use_settings(Some(settings.clone()));
        assert_eq!(workspace.layout(), Layout::EggDb);
        let now = Instant::now();
        workspace.act(ConnectAction::ShowLayoutPicker, now);
        assert!(card(&mut workspace).layout_picker.is_some());
        assert!(
            card_tree(&workspace)
                .iter()
                .any(|node| node.id == layout_picker::NODE_DIALOG)
        );
        workspace.key(KeyPress::plain(Key::Down), now);
        workspace.key(KeyPress::plain(Key::Escape), now);
        assert!(card(&mut workspace).layout_picker.is_none());
        assert_eq!(workspace.layout(), Layout::EggDb);
        workspace.act(ConnectAction::ShowLayoutPicker, now);
        workspace.key(KeyPress::plain(Key::Down), now);
        workspace.key(KeyPress::plain(Key::Down), now);
        workspace.key(KeyPress::plain(Key::Enter), now);
        assert_eq!(workspace.layout(), Layout::PgAdmin);
        assert_eq!(card(&mut workspace).layout, Layout::PgAdmin);
        assert_eq!(
            std::fs::read_to_string(&settings).expect("settings are kept"),
            "layout = pgadmin\ntheme = dark\n"
        );
        let mut again = workspace_with_settings(&directory, &settings);
        assert_eq!(again.layout(), Layout::PgAdmin);
        again.open_typed("layouts.eggdb", now);
        again.wait_for_open(Duration::from_secs(30));
        let window = again.main_window_mut().expect("the main window shows");
        assert_eq!(window.layout, Layout::PgAdmin);
        let primary = crate::accessibility::primary();
        again.key(
            KeyPress {
                key: Key::Character('k'),
                modifiers: primary,
            },
            now,
        );
        again.key(KeyPress::plain(Key::Character('l')), now);
        again.key(KeyPress::plain(Key::Up), now);
        again.key(KeyPress::plain(Key::Enter), now);
        assert_eq!(again.layout(), Layout::Ssms);
        assert_eq!(ClientSettings::load(&settings).layout, Some(Layout::Ssms));
    }

    fn workspace_with_settings(directory: &Path, settings: &Path) -> Workspace {
        let mut workspace = workspace(Arc::new(EngineConnector), directory.to_path_buf());
        workspace.use_settings(Some(settings.to_path_buf()));
        workspace
    }

    #[test]
    fn a_press_on_the_picker_s_scrim_keeps_the_layout_and_a_double_click_applies_a_row() {
        let directory = scratch("layout-pointer");
        let mut workspace = workspace(Arc::new(EngineConnector), directory);
        let faces = crate::main_window::Faces {
            ui: &crate::font::BoxFace,
            ui_bold: &crate::font::BoxFace,
            mono: &crate::font::BoxFace,
        };
        let size = (
            crate::connect_screen::DEFAULT_WIDTH,
            crate::connect_screen::DEFAULT_HEIGHT,
        );
        let now = Instant::now();
        let press = |workspace: &mut Workspace, x: f32, y: f32, at: Instant| {
            workspace.pointer_pressed(
                x,
                y,
                PointerButton::Primary,
                Modifiers::default(),
                faces,
                size,
                at,
            )
        };
        workspace.act(ConnectAction::ShowLayoutPicker, now);
        press(&mut workspace, 10.0, 10.0, now);
        assert!(card(&mut workspace).layout_picker.is_none());
        workspace.act(ConnectAction::ShowLayoutPicker, now);
        let row = layout_picker::row_rect((1280.0, 800.0), 7);
        let later = now + Duration::from_secs(5);
        press(&mut workspace, row.x + 10.0, row.y + 10.0, later);
        press(
            &mut workspace,
            row.x + 10.0,
            row.y + 10.0,
            later + Duration::from_millis(50),
        );
        assert_eq!(workspace.layout(), Layout::RedisInsight);
        assert!(card(&mut workspace).layout_picker.is_none());
    }
}
