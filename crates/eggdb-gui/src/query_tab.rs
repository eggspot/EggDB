use std::ops::Range;
use std::time::{Duration, Instant};

use eggdb::sql::TransactionStatus;
use eggdb_platform::window::{Key, KeyPress};

use crate::connection::{ResultTable, SessionId, StatementFailure};
use crate::dialect::QueryDialect;
use crate::editor::{self, Editor, Position};
use crate::editor_menu::{EditorMenu, EditorMenuAnswer, EditorMenuItem, EditorMenuOrigin, EditorState};
use crate::grid::{GridAnswer, GridMenuItem, ResultGrid, cell_text, column_widths};
use crate::grid_menu::{GridMenu, MenuAnswer};
use crate::key_scan::{KEY_SCAN_LIMIT, KeyPages, KeySpaceScan, PageEnd, PageStart, unspelled};
use crate::plan_view::{Plan, PlanAnswer, PlanView, explain_statements};
use crate::sql_format::{comment_edits, format_sql, is_blank_sql};
use crate::statements::{source_table, statement_at, statements};
use crate::status_line;
use crate::value_view::{ValueAnswer, ValueView};

const FIX_LABEL_CHARACTERS: usize = 40;
const CANCELLED: &str = "57014";

/// The rows the results body shows at the golden frame's size, for paging keys.
pub const ROWS_IN_VIEW: usize = 13;
/// What the status bar says when a run key is pressed while a statement runs.
pub const STILL_RUNNING: &str = "Running… press Esc to stop";
/// The status bar's item while the tab's transaction is open.
pub const TRANSACTION_OPEN: &str = "Transaction open";
/// The status bar's item while the tab's transaction failed.
pub const TRANSACTION_FAILED: &str = "Transaction failed: ROLLBACK to continue";

/// The plan panel's width and height at the golden frame's size, for keeping the current
/// node in view.
pub const PLAN_PANEL: (f32, f32) = (1020.0, 358.0);

/// How long after the last key in a keys tab's prefix field the scan runs again.
pub const PREFIX_DEBOUNCE: Duration = Duration::from_millis(150);

/// Where the keyboard focus is in the query tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabFocus {
    /// The editor.
    #[default]
    Editor,
    /// The toolbar's dialect pill.
    DialectPill,
    /// The results grid.
    Grid,
    /// The plan's tree.
    Plan,
}

/// The results header's segment that is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResultView {
    /// The grid.
    #[default]
    Results,
    /// The statements' tag lines and the engine's error text.
    Messages,
    /// The last `Explain`'s plan.
    Plan,
}

/// An error from the engine shown in the editor (`docs/ux/workspace.md` 2.5): the squiggle
/// under its range and the strip at the editor's bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineError {
    /// The SQLSTATE, such as `42P01`.
    pub code: String,
    /// The engine's message.
    pub message: String,
    /// The bytes of the editor's text the squiggle is under; `None` when the text changed
    /// while the statement ran, so the run's offsets no longer point into it.
    pub range: Option<Range<usize>>,
    /// The 1-based line of the error's position.
    pub line: usize,
    /// The 1-based column, in characters.
    pub column: usize,
    /// The engine's fix, when it gave one.
    pub fix: Option<String>,
    /// Whether the strip shows; `Esc` hides it while the squiggle stays.
    pub strip_shown: bool,
}

impl InlineError {
    /// The strip's text: `Error 42P01: relation "users" does not exist · Ln 1, Col 18`.
    pub fn strip_text(&self) -> String {
        let code = if self.code.is_empty() {
            String::new()
        } else {
            format!(" {}", self.code)
        };
        format!(
            "Error{code}: {} · Ln {}, Col {}",
            self.message, self.line, self.column
        )
    }

    /// The fix button's label: `Fix: ` and the fix's first 40 characters.
    pub fn fix_label(&self) -> Option<String> {
        let fix = self.fix.as_deref()?;
        let fix = fix.strip_prefix("Try: ").unwrap_or(fix);
        Some(format!(
            "Fix: {}",
            fix.chars().take(FIX_LABEL_CHARACTERS).collect::<String>()
        ))
    }
}

/// What a run runs: the statements' texts, where each starts in the editor's text, and the
/// dialect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    /// The statements, in order.
    pub statements: Vec<String>,
    /// The byte offset in the editor's text where each statement starts.
    pub starts: Vec<usize>,
    /// The dialect they run in.
    pub dialect: QueryDialect,
    /// Whether this is `Run all`.
    pub all: bool,
    /// What `Explain` explains, when the run is one.
    pub explain: Option<ExplainRun>,
    /// The editor's text when the run was asked for, which `starts` are offsets into.
    pub source_text: String,
    /// The tab's kept session the statements run on.
    pub session: SessionId,
    /// For a keys tab, the key space to scan instead of running the statements
    /// (`docs/ux/workspace.md` 10.4).
    pub scan: Option<KeySpaceScan>,
}

/// The statement an `Explain` run explains and how its answer is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainRun {
    /// Whether it runs the statement (`EXPLAIN ANALYZE`).
    pub analyze: bool,
    /// The statement's bytes in the editor's text.
    pub statement: Range<usize>,
    /// The statement's text.
    pub statement_text: String,
    /// What runs before the statements, such as `SET SHOWPLAN_ALL ON`.
    pub before: Vec<String>,
    /// What runs after them whatever happened, such as `SET SHOWPLAN_ALL OFF`.
    pub after: Vec<String>,
    /// Which of the run's outcomes answers with the plan.
    pub plan_at: usize,
    /// Which of the run's outcomes is the explained statement's own.
    pub statement_at: usize,
    /// How many bytes were put before the statement.
    pub prefix_len: usize,
    /// Whether the run goes through [`crate::connection::Connection::run_rolled_back`], so
    /// nothing it writes stays.
    pub rolled_back: bool,
}

/// What the window must do after a key in the query tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabAction {
    /// Nothing beyond drawing the new frame.
    None,
    /// Run these statements off the window's thread.
    Run(RunRequest),
    /// Stop the running statement.
    Stop,
    /// Put `text` on the clipboard and show `toast`.
    Copy {
        /// The copied text.
        text: String,
        /// What the toast says.
        toast: String,
    },
    /// Read the clipboard and give its text to [`QueryTab::paste`].
    Paste,
}

/// The toast after copying text from the editor.
pub fn copied_characters_toast(count: usize) -> String {
    if count == 1 {
        "Copied 1 character".to_owned()
    } else {
        format!("Copied {count} characters")
    }
}

/// One query tab (`docs/ux/workspace.md` 2): its editor and dialect, the last run's results,
/// messages and error, and the strings the status line and status bar show.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryTab {
    /// The tab's title, `Query 1`.
    pub title: String,
    /// The editor.
    pub editor: Editor,
    /// The dialect the editor highlights and the next run uses.
    pub dialect: QueryDialect,
    /// The highlighted row of the open dialect menu; `None` while it is closed.
    pub dialect_menu: Option<usize>,
    /// Where the focus is.
    pub focus: TabFocus,
    /// The results header's segment that shows.
    pub view: ResultView,
    /// The last result set.
    pub grid: Option<ResultGrid>,
    /// The `Messages` segment's lines.
    pub messages: Vec<String>,
    /// The last run's error, while it shows.
    pub error: Option<InlineError>,
    /// The results header's summary, such as `3 rows · 0.4 ms · 1.2 KB`.
    pub summary: String,
    /// The status bar's left group, such as `SELECT 3 · 0.4 ms`.
    pub status_tags: String,
    /// A short notice that takes the status bar's left group for a while: a toast such as
    /// `Copied 12 cells`, or `Running… press Esc to stop`.
    pub notice: Option<String>,
    /// Whether a statement runs.
    pub running: bool,
    /// Whether `Stop` was pressed and the engine has not confirmed yet.
    pub stopping: bool,
    /// The last `Explain`'s plan.
    pub plan: Option<PlanView>,
    /// The session this tab's runs keep (`docs/ux/workspace.md` 9.2); the main window gives
    /// every tab its own.
    pub session: SessionId,
    /// Where the tab's kept session stands toward transactions after its last run.
    pub transaction: TransactionStatus,
    /// The grid's open full-value view (`docs/ux/workspace.md` 9.3).
    pub value_view: Option<ValueView>,
    /// The grid's open right-click menu (`docs/ux/workspace.md` 9.4).
    pub grid_menu: Option<GridMenu>,
    /// The key space a keys tab scans; `None` for a query tab (`docs/ux/workspace.md` 10.4).
    pub keys: Option<KeySpaceScan>,
    /// The editor's open right-click menu (`docs/ux/workspace.md` 10.9).
    pub editor_menu: Option<EditorMenu>,
    /// A keys tab's prefix field and pages (`docs/ux/workspace.md` 10.10).
    pub key_pages: KeyPages,
}

impl QueryTab {
    /// An empty tab titled `title` in PostgreSQL's dialect.
    pub fn new(title: &str) -> Self {
        Self::with_text(title, "")
    }

    /// A tab titled `title` holding `text`, the caret at its end.
    pub fn with_text(title: &str, text: &str) -> Self {
        Self {
            title: title.to_owned(),
            editor: Editor::new(text),
            dialect: QueryDialect::PostgreSql,
            dialect_menu: None,
            focus: TabFocus::Editor,
            view: ResultView::Results,
            grid: None,
            messages: Vec::new(),
            error: None,
            summary: String::new(),
            status_tags: String::new(),
            notice: None,
            running: false,
            stopping: false,
            plan: None,
            session: SessionId::default(),
            transaction: TransactionStatus::Idle,
            value_view: None,
            grid_menu: None,
            keys: None,
            editor_menu: None,
            key_pages: KeyPages::default(),
        }
    }

    /// A keys tab on `scan`'s key space: titled `Keys in users`, its read-only editor holding
    /// the scan's one line.
    pub fn keys(scan: KeySpaceScan) -> Self {
        Self {
            keys: Some(scan.clone()),
            ..Self::with_text(&scan.title(), &scan.statement())
        }
    }

    /// Whether this is a keys tab, whose editor is read-only and whose dialect is fixed.
    pub fn is_keys(&self) -> bool {
        self.keys.is_some()
    }

    /// What the dialect pill and the status bar name: the dialect, or `Keys` in a keys tab.
    pub fn pill_label(&self) -> &'static str {
        if self.is_keys() {
            "Keys"
        } else {
            self.dialect.label()
        }
    }

    /// Switches the dialect: the text, caret, selection and undo history stay, the editor
    /// re-highlights, and the next run uses it.
    pub fn set_dialect(&mut self, dialect: QueryDialect) {
        self.dialect = dialect;
        if let Some(grid) = &mut self.grid {
            grid.dialect = dialect;
        }
    }

    /// What `Run` (or, with `all`, `Run all`) runs now: the selection's statements, else the
    /// statement under the caret; every statement for `Run all`. `None` when there is
    /// nothing to run.
    pub fn run_request(&self, all: bool) -> Option<RunRequest> {
        let text = self.editor.text();
        if let Some(scan) = &self.keys {
            if all {
                return None;
            }
            let prefix = unspelled(&self.key_pages.prefix_text).ok()?;
            return Some(self.scan_request(KeySpaceScan {
                prefix,
                start: PageStart::First,
                first_position: 1,
                more: false,
                ..scan.clone()
            }));
        }
        let ranges: Vec<Range<usize>> = if all {
            statements(&text, self.dialect)
        } else if let Some(selected) = self.editor.selected_bytes() {
            statements(&text[selected.clone()], self.dialect)
                .into_iter()
                .map(|range| range.start + selected.start..range.end + selected.start)
                .collect()
        } else {
            let caret = self.editor.offset(self.editor.caret());
            statement_at(&text, caret, self.dialect)
                .into_iter()
                .collect()
        };
        if ranges.is_empty() {
            return None;
        }
        Some(RunRequest {
            statements: ranges
                .iter()
                .map(|range| text[range.clone()].to_owned())
                .collect(),
            starts: ranges.iter().map(|range| range.start).collect(),
            dialect: self.dialect,
            all,
            explain: None,
            source_text: text,
            session: self.session,
            scan: None,
        })
    }

    /// What `Explain` (or, with `analyze`, `Explain (analyze)`) runs now: the first
    /// statement of the selection, else the statement under the caret, explained in the
    /// tab's dialect's own form ([`explain_statements`]). The error is the `Messages` text
    /// for what EggDB cannot explain in the dialect yet.
    pub fn explain_request(&self, analyze: bool) -> Result<Option<RunRequest>, String> {
        if self.is_keys() {
            return Ok(None);
        }
        let Some(base) = self.run_request(false) else {
            return Ok(None);
        };
        let (Some(statement), Some(&start)) = (base.statements.first(), base.starts.first()) else {
            return Ok(None);
        };
        let explained = explain_statements(self.dialect, statement, analyze)?;
        let count = explained.statements.len();
        Ok(Some(RunRequest {
            statements: explained.statements,
            starts: vec![start; count],
            dialect: self.dialect,
            all: false,
            source_text: base.source_text.clone(),
            session: self.session,
            scan: None,
            explain: Some(ExplainRun {
                analyze,
                statement: start..start + statement.len(),
                statement_text: statement.clone(),
                before: explained.before,
                after: explained.after,
                plan_at: explained.plan_at,
                statement_at: explained.statement_at,
                prefix_len: explained.prefix_len,
                rolled_back: explained.rolled_back,
            }),
        }))
    }

    /// `Ctrl/Cmd+E` (with `analyze`, `Ctrl/Cmd+Shift+E`): the run to start, or `Messages`
    /// showing why the dialect cannot explain yet.
    pub fn explain(&mut self, analyze: bool) -> TabAction {
        if self.running {
            self.notice = Some(STILL_RUNNING.to_owned());
            return TabAction::None;
        }
        match self.explain_request(analyze) {
            Ok(Some(request)) => TabAction::Run(request),
            Ok(None) => TabAction::None,
            Err(message) => {
                self.messages = vec![message];
                self.view = ResultView::Messages;
                TabAction::None
            }
        }
    }

    /// Answers a key (`docs/ux/workspace.md` 2.2, 2.3, 3.3 to 3.5).
    pub fn key(&mut self, press: KeyPress) -> TabAction {
        let primary = press.modifiers.control || press.modifiers.command;
        if let Some(highlighted) = self.dialect_menu {
            self.menu_key(press, highlighted);
            return TabAction::None;
        }
        if let Some(view) = &mut self.value_view {
            return match view.key(press) {
                ValueAnswer::Moved => TabAction::None,
                ValueAnswer::Close => {
                    self.value_view = None;
                    TabAction::None
                }
                ValueAnswer::Copy { text, toast } => {
                    self.notice = Some(toast.clone());
                    TabAction::Copy { text, toast }
                }
            };
        }
        if let Some(menu) = &mut self.editor_menu {
            return match menu.key(press) {
                EditorMenuAnswer::Moved => TabAction::None,
                EditorMenuAnswer::Close => {
                    self.editor_menu = None;
                    TabAction::None
                }
                EditorMenuAnswer::Activate(item) => self.editor_menu_item(item),
            };
        }
        if let Some(menu) = &mut self.grid_menu {
            return match menu.key(press) {
                MenuAnswer::Moved => TabAction::None,
                MenuAnswer::Close => {
                    self.grid_menu = None;
                    TabAction::None
                }
                MenuAnswer::Activate(item) => self.menu_item(item),
            };
        }
        let run_all = primary && press.modifiers.shift;
        match press.key {
            Key::Enter if primary => return self.run_key(run_all),
            Key::Character('e' | 'E') if primary => return self.explain(press.modifiers.shift),
            Key::Character('t' | 'T')
                if press.modifiers.alt && !primary && self.view == ResultView::Plan =>
            {
                if let Some(plan) = &mut self.plan {
                    plan.text_shown = !plan.text_shown;
                }
                return TabAction::None;
            }
            Key::F5 => return self.run_key(false),
            Key::Character('.') if press.modifiers.command && self.running => {
                return TabAction::Stop;
            }
            Key::Escape if self.running && self.focus == TabFocus::Editor => {
                return TabAction::Stop;
            }
            Key::Character('3') if primary => {
                if self.view == ResultView::Plan && self.plan.is_some() {
                    self.focus_plan();
                } else if self.grid.is_some() {
                    self.focus_grid();
                }
                return TabAction::None;
            }
            _ => {}
        }
        match self.focus {
            TabFocus::Editor | TabFocus::DialectPill if self.is_keys() => TabAction::None,
            TabFocus::Editor => self.editor_key(press),
            TabFocus::DialectPill => self.pill_key(press),
            TabFocus::Grid => self.grid_key(press),
            TabFocus::Plan => self.plan_key(press),
        }
    }

    fn run_key(&mut self, all: bool) -> TabAction {
        if self.running {
            self.notice = Some(STILL_RUNNING.to_owned());
            return TabAction::None;
        }
        self.run_request(all)
            .map_or(TabAction::None, TabAction::Run)
    }

    fn focus_grid(&mut self) {
        self.focus = TabFocus::Grid;
        self.view = ResultView::Results;
        if let Some(grid) = &mut self.grid {
            grid.focused = true;
        }
        if let Some(plan) = &mut self.plan {
            plan.focused = false;
        }
    }

    fn focus_plan(&mut self) {
        self.focus = TabFocus::Plan;
        if let Some(plan) = &mut self.plan {
            plan.focused = true;
        }
        if let Some(grid) = &mut self.grid {
            grid.focused = false;
        }
    }

    /// Moves the focus to `focus`: the grid or the plan only when they show something,
    /// the editor otherwise.
    pub fn focus(&mut self, focus: TabFocus) {
        let results = matches!(focus, TabFocus::Grid | TabFocus::Plan);
        if results && self.view == ResultView::Plan && self.plan.is_some() {
            self.focus_plan();
            return;
        }
        if results && self.grid.is_some() {
            self.focus_grid();
            return;
        }
        self.focus = if results { TabFocus::Editor } else { focus };
        if let Some(grid) = &mut self.grid {
            grid.focused = false;
        }
        if let Some(plan) = &mut self.plan {
            plan.focused = false;
        }
    }

    fn plan_key(&mut self, press: KeyPress) -> TabAction {
        let Some(plan) = &mut self.plan else {
            self.focus(TabFocus::Editor);
            return TabAction::None;
        };
        match plan.key(press, PLAN_PANEL) {
            PlanAnswer::LeaveForward => self.focus(TabFocus::DialectPill),
            PlanAnswer::LeaveBack => self.focus(TabFocus::Editor),
            PlanAnswer::Moved
            | PlanAnswer::Selected
            | PlanAnswer::Cleared
            | PlanAnswer::Tooltip
            | PlanAnswer::Ignored => {}
        }
        TabAction::None
    }

    fn menu_key(&mut self, press: KeyPress, highlighted: usize) {
        let last = QueryDialect::ALL.len() - 1;
        match press.key {
            Key::Up => self.dialect_menu = Some(highlighted.saturating_sub(1)),
            Key::Down => self.dialect_menu = Some((highlighted + 1).min(last)),
            Key::Home => self.dialect_menu = Some(0),
            Key::End => self.dialect_menu = Some(last),
            Key::Enter | Key::Space => {
                let picked = QueryDialect::at(highlighted);
                if picked == self.dialect || self.dialect_switch_allowed() {
                    self.set_dialect(picked);
                    self.dialect_menu = None;
                }
            }
            Key::Escape | Key::Tab => self.dialect_menu = None,
            Key::Character(letter) => {
                if let Some(found) = QueryDialect::starting_with(letter, highlighted) {
                    self.dialect_menu = Some(found);
                }
            }
            _ => {}
        }
    }

    /// Whether the dialect pill may switch: not while the tab's transaction is open or
    /// failed (`docs/ux/workspace.md` 9.2), since its session keeps the dialect it began in.
    pub fn dialect_switch_allowed(&self) -> bool {
        self.transaction == TransactionStatus::Idle
    }

    /// The toast the dialect menu shows when it opens while the switch is refused.
    pub fn dialect_locked_toast(&self) -> String {
        format!(
            "Warning: COMMIT or ROLLBACK first; {} has a transaction open",
            self.title
        )
    }

    /// The tab's name for screen readers: `Query 2`, with `, running`, `, transaction open`
    /// or `, transaction failed` while so.
    pub fn spoken_title(&self) -> String {
        let state = match (self.running, self.transaction) {
            (true, _) => ", running",
            (false, TransactionStatus::InBlock) => ", transaction open",
            (false, TransactionStatus::Failed) => ", transaction failed",
            (false, TransactionStatus::Idle) => "",
        };
        format!("{}{state}", self.title)
    }

    /// The status bar's transaction item for this tab, with whether it reads as failed.
    pub fn transaction_item(&self) -> Option<(&'static str, bool)> {
        match self.transaction {
            TransactionStatus::InBlock => Some((TRANSACTION_OPEN, false)),
            TransactionStatus::Failed => Some((TRANSACTION_FAILED, true)),
            TransactionStatus::Idle => None,
        }
    }

    /// Opens the dialect menu on the current dialect; while the switch is refused it says why.
    pub fn open_dialect_menu(&mut self) {
        if self.is_keys() {
            return;
        }
        self.dialect_menu = Some(self.dialect.index());
        if !self.dialect_switch_allowed() {
            self.notice = Some(self.dialect_locked_toast());
        }
    }

    fn pill_key(&mut self, press: KeyPress) -> TabAction {
        match press.key {
            Key::Space | Key::Enter => self.open_dialect_menu(),
            Key::Down if press.modifiers.alt => self.open_dialect_menu(),
            Key::Tab if press.modifiers.shift => self.focus(TabFocus::Grid),
            Key::Tab => self.focus(TabFocus::Editor),
            _ => {}
        }
        TabAction::None
    }

    /// Opens the full-value view on the grid's current cell (`docs/ux/workspace.md` 9.3).
    pub fn open_value(&mut self) {
        self.grid_menu = None;
        self.value_view = self
            .grid
            .as_ref()
            .filter(|grid| grid.current.row < grid.rows.len())
            .map(|grid| ValueView::open(grid, grid.current));
    }

    /// Opens the grid's right-click menu `menu`, closing the value view.
    pub fn open_grid_menu(&mut self, menu: GridMenu) {
        if self.grid.is_some() {
            self.value_view = None;
            self.grid_menu = Some(menu);
        }
    }

    /// Closes the grid's menu and does what its `item` says.
    pub fn menu_item(&mut self, item: GridMenuItem) -> TabAction {
        self.grid_menu = None;
        let Some(grid) = &mut self.grid else {
            return TabAction::None;
        };
        let answer = grid.menu_item(item);
        self.grid_answer(answer)
    }

    fn grid_key(&mut self, press: KeyPress) -> TabAction {
        let Some(grid) = &mut self.grid else {
            self.focus(TabFocus::Editor);
            return TabAction::None;
        };
        let answer = grid.key(press, ROWS_IN_VIEW);
        self.grid_answer(answer)
    }

    fn grid_answer(&mut self, answer: GridAnswer) -> TabAction {
        match answer {
            GridAnswer::Copy { text, toast } => {
                self.notice = Some(toast.clone());
                TabAction::Copy { text, toast }
            }
            GridAnswer::LeaveForward => {
                self.focus(TabFocus::DialectPill);
                TabAction::None
            }
            GridAnswer::LeaveBack => {
                self.focus(TabFocus::Editor);
                TabAction::None
            }
            GridAnswer::Moved => {
                if let Some(grid) = &mut self.grid {
                    grid.scroll_into_view(ROWS_IN_VIEW, crate::main_window::GRID_BODY_WIDTH);
                }
                TabAction::None
            }
            GridAnswer::OpenValue => {
                self.open_value();
                TabAction::None
            }
            GridAnswer::OpenMenu => {
                self.open_grid_menu(GridMenu::at_current_cell());
                TabAction::None
            }
            GridAnswer::Resized => {
                if let Some(grid) = &self.grid {
                    self.notice = Some(grid.width_text(grid.current.column));
                }
                TabAction::None
            }
            GridAnswer::Ignored => TabAction::None,
        }
    }

    fn editor_key(&mut self, press: KeyPress) -> TabAction {
        if press.key == Key::Escape {
            if let Some(error) = &mut self.error {
                error.strip_shown = false;
            }
            return TabAction::None;
        }
        if press.key == Key::Tab && press.modifiers.shift {
            self.focus(TabFocus::DialectPill);
            return TabAction::None;
        }
        let primary = press.modifiers.control || press.modifiers.command;
        match press.key {
            Key::Character('c' | 'C' | 'x' | 'X') if primary && !press.modifiers.shift => {
                let Some(text) = self.editor.selected_text() else {
                    return TabAction::None;
                };
                if matches!(press.key, Key::Character('x' | 'X')) {
                    self.editor.apply(editor::Key::Backspace);
                    self.edited(self.editor.caret().line);
                }
                let toast = copied_characters_toast(text.chars().count());
                return TabAction::Copy { text, toast };
            }
            Key::Character('v' | 'V') if primary && !press.modifiers.shift => {
                return TabAction::Paste;
            }
            Key::Character('/') if primary => {
                self.toggle_comment();
                return TabAction::None;
            }
            Key::Character('f' | 'F') if primary && press.modifiers.shift => {
                self.format_statement();
                return TabAction::None;
            }
            _ => {}
        }
        let Some(key) = editor_key(press) else {
            return TabAction::None;
        };
        let before = self.editor.text();
        let line = self.editor.caret().line;
        self.editor.apply(key);
        if self.editor.text() != before {
            self.edited(line);
        }
        TabAction::None
    }

    fn edited(&mut self, line: usize) {
        if self
            .error
            .as_ref()
            .is_some_and(|error| error.line == line + 1)
        {
            self.error = None;
        }
        if let Some(plan) = self.plan.as_mut() {
            plan.highlight = None;
            plan.selected = None;
        }
    }

    /// Inserts `text` at the editor's caret, replacing the selection (`docs/ux/workspace.md`
    /// 6.3): `text` is already cleaned by [`crate::pointer::paste_text`].
    pub fn paste(&mut self, text: &str) {
        if self.is_keys() {
            return;
        }
        let line = self.editor.caret().line;
        self.editor.paste(text);
        self.edited(line);
    }

    /// What the editor holds, for its right-click menu's rules (`docs/ux/workspace.md`
    /// 10.9); `clipboard_text` says whether the clipboard holds text.
    pub fn editor_state(&self, clipboard_text: bool) -> EditorState {
        let text = self.editor.text();
        let keys = self.is_keys();
        EditorState {
            selection: !keys && self.editor.selection().is_some(),
            read_only: keys,
            empty: text.is_empty(),
            clipboard_text,
            running: self.running,
            blank_range: self.run_range().is_none_or(|range| {
                is_blank_sql(text.get(range).unwrap_or(""), self.dialect)
            }),
        }
    }

    fn run_range(&self) -> Option<Range<usize>> {
        if let Some(selected) = self.editor.selected_bytes() {
            return Some(selected);
        }
        let text = self.editor.text();
        statement_at(&text, self.editor.offset(self.editor.caret()), self.dialect)
    }

    /// Opens the editor's right-click menu at `origin`, closing the other popovers.
    pub fn open_editor_menu(&mut self, origin: EditorMenuOrigin, clipboard_text: bool) {
        self.grid_menu = None;
        self.value_view = None;
        self.dialect_menu = None;
        self.editor_menu = Some(EditorMenu::new(self.editor_state(clipboard_text), origin));
    }

    /// Closes the editor's menu and does what its `item` says (`docs/ux/workspace.md` 10.9).
    pub fn editor_menu_item(&mut self, item: EditorMenuItem) -> TabAction {
        self.editor_menu = None;
        let keys = self.is_keys();
        match item {
            EditorMenuItem::Cut | EditorMenuItem::Copy => {
                let text = if keys {
                    Some(self.editor.text())
                } else {
                    self.editor.selected_text()
                };
                let Some(text) = text else {
                    return TabAction::None;
                };
                if item == EditorMenuItem::Cut {
                    if keys {
                        return TabAction::None;
                    }
                    self.editor.apply(editor::Key::Backspace);
                    self.edited(self.editor.caret().line);
                }
                let toast = copied_characters_toast(text.chars().count());
                self.notice = Some(toast.clone());
                TabAction::Copy { text, toast }
            }
            EditorMenuItem::Paste if !keys => TabAction::Paste,
            EditorMenuItem::SelectAll if !keys => {
                self.editor.apply(editor::Key::SelectAll);
                TabAction::None
            }
            EditorMenuItem::Run => self.run_key(false),
            EditorMenuItem::Explain if !keys => self.explain(false),
            EditorMenuItem::ToggleComment => {
                self.toggle_comment();
                TabAction::None
            }
            EditorMenuItem::FormatStatement => {
                self.format_statement();
                TabAction::None
            }
            EditorMenuItem::Paste | EditorMenuItem::SelectAll | EditorMenuItem::Explain => {
                TabAction::None
            }
        }
    }

    /// `Toggle comment` (`Ctrl/Cmd+/`, `docs/ux/workspace.md` 10.9) on the selection's lines,
    /// a selection ending at a line's start leaving that line out, or the caret's line; one
    /// undo step, the selection growing or shrinking with the text.
    pub fn toggle_comment(&mut self) {
        if self.is_keys() || self.editor.text().is_empty() {
            return;
        }
        let caret = self.editor.caret();
        let selection = self.editor.selection();
        let (first, last) = match selection {
            Some((start, end)) if end.line > start.line && end.column == 0 => {
                (start.line, end.line - 1)
            }
            Some((start, end)) => (start.line, end.line),
            None => (caret.line, caret.line),
        };
        let lines: Vec<&str> = self
            .editor
            .lines()
            .iter()
            .skip(first)
            .take(last + 1 - first)
            .map(String::as_str)
            .collect();
        let edits = comment_edits(&lines);
        if edits.iter().all(Option::is_none) {
            return;
        }
        let replaced: Vec<String> = lines
            .iter()
            .zip(&edits)
            .map(|(line, edit)| edit.map_or_else(|| (*line).to_owned(), |edit| edit.applied(line)))
            .collect();
        let moved = |at: Position| Position {
            line: at.line,
            column: at
                .line
                .checked_sub(first)
                .and_then(|index| edits.get(index).copied().flatten())
                .map_or(at.column, |edit| edit.moved(at.column)),
        };
        let anchor = selection.map(|(start, end)| if caret == end { start } else { end });
        let start = self.editor.offset(Position {
            line: first,
            column: 0,
        });
        let end = self.editor.offset(Position {
            line: last,
            column: usize::MAX,
        });
        self.editor.replace_bytes(start..end, &replaced.join("\n"));
        match anchor {
            Some(anchor) => self.editor.set_selection(moved(anchor), moved(caret)),
            None => self.editor.place_caret(moved(caret)),
        }
        self.edited(first);
    }

    /// `Format statement` (`Ctrl/Cmd+Shift+F`, `docs/ux/workspace.md` 10.9): lays out the
    /// selection, else the statement under the caret, as one undo step with the caret at its
    /// start and `Formatted 1 statement` in the status bar; a range that does not lex stays
    /// and the status bar says why.
    pub fn format_statement(&mut self) {
        if self.is_keys() || self.running {
            return;
        }
        let Some(range) = self.run_range() else {
            return;
        };
        let text = self.editor.text();
        let Some(source) = text.get(range.clone()) else {
            return;
        };
        if is_blank_sql(source, self.dialect) {
            return;
        }
        let start = self.editor.position_at(range.start);
        let indent = self
            .editor
            .lines()
            .get(start.line)
            .map_or(0, |line| line.chars().take_while(|c| *c == ' ').count());
        match format_sql(source, self.dialect, indent) {
            Ok(formatted) => {
                self.editor.replace_bytes(range, &formatted.text);
                self.editor.place_caret(start);
                self.notice = Some(formatted.notice());
                self.edited(start.line);
            }
            Err(error) => {
                let at = self.editor.position_at(range.start + error.at);
                self.notice = Some(format!(
                    "Cannot format: {} at Ln {}, Col {}",
                    error.what,
                    at.line + 1,
                    at.column + 1
                ));
            }
        }
    }

    fn scan_request(&self, scan: KeySpaceScan) -> RunRequest {
        let text = scan.statement();
        RunRequest {
            statements: vec![text.clone()],
            starts: vec![0],
            dialect: self.dialect,
            all: false,
            explain: None,
            source_text: text,
            session: self.session,
            scan: Some(scan),
        }
    }

    fn page_request(
        &mut self,
        start: PageStart,
        first_position: usize,
        earlier: Vec<PageStart>,
        land_on_last: bool,
    ) -> TabAction {
        if self.running {
            self.notice = Some(STILL_RUNNING.to_owned());
            return TabAction::None;
        }
        let prefix = unspelled(&self.key_pages.prefix_text).ok();
        let (Some(keys), Some(prefix)) = (&self.keys, prefix) else {
            return TabAction::None;
        };
        let scan = KeySpaceScan {
            prefix,
            start,
            first_position,
            more: false,
            ..keys.clone()
        };
        self.key_pages.pending_earlier = Some(earlier);
        self.key_pages.land_on_last = land_on_last;
        self.key_pages.due = None;
        TabAction::Run(self.scan_request(scan))
    }

    /// `Next 100` (`Alt+PgDn`, `docs/ux/workspace.md` 10.10): scans from after the last key
    /// shown; nothing on the last page.
    pub fn next_page(&mut self) -> TabAction {
        let (Some(keys), Some(next)) = (&self.keys, self.key_pages.next.clone()) else {
            return TabAction::None;
        };
        let shown = self.grid.as_ref().map_or(0, |grid| grid.rows.len());
        let mut earlier = self.key_pages.earlier.clone();
        earlier.push(keys.start.clone());
        let position = keys.first_position.saturating_add(shown);
        self.page_request(next, position, earlier, false)
    }

    /// `Back` (`Alt+PgUp`): the previous page from its recorded start; nothing on page 1.
    pub fn previous_page(&mut self) -> TabAction {
        let Some(keys) = &self.keys else {
            return TabAction::None;
        };
        let position = keys.first_position.saturating_sub(KEY_SCAN_LIMIT).max(1);
        let mut earlier = self.key_pages.earlier.clone();
        let Some(start) = earlier.pop() else {
            return TabAction::None;
        };
        self.page_request(start, position, earlier, true)
    }

    /// Page 1 with the prefix the field holds: what `Enter` in the field and a changed
    /// prefix run.
    pub fn first_page(&mut self) -> TabAction {
        self.page_request(PageStart::First, 1, Vec::new(), false)
    }

    /// Focuses the keys tab's prefix field with its text selected.
    pub fn focus_prefix(&mut self) {
        if !self.is_keys() {
            return;
        }
        let pages = &mut self.key_pages;
        pages.prefix_focused = true;
        pages.prefix_selected = true;
        pages.prefix_on_focus = pages.prefix_text.clone();
        if let Some(grid) = &mut self.grid {
            grid.focused = false;
        }
    }

    fn leave_prefix(&mut self) {
        self.key_pages.prefix_focused = false;
        self.key_pages.prefix_selected = false;
        self.focus(TabFocus::Grid);
    }

    /// The keys tab's own keys (`docs/ux/workspace.md` 10.10), before the tab's others: the
    /// prefix field's typing, `/` from the grid or the editor and `Ctrl/Cmd+F` to focus it,
    /// `Alt+PgDn` / `Alt+PgUp`, and `PgDn` / `PgUp` past the page's edge in the grid. `None`
    /// leaves the key to the tab.
    pub fn keys_tab_key(&mut self, press: KeyPress, now: Instant) -> Option<TabAction> {
        if !self.is_keys()
            || self.editor_menu.is_some()
            || self.grid_menu.is_some()
            || self.value_view.is_some()
        {
            return None;
        }
        if self.key_pages.prefix_focused {
            return Some(self.prefix_key(press, now));
        }
        let primary = press.modifiers.control || press.modifiers.command;
        let in_grid = self.focus == TabFocus::Grid;
        let (on_last_row, on_first_row) = self.grid.as_ref().map_or((false, false), |grid| {
            (
                grid.current.row + 1 >= grid.rows.len(),
                grid.current.row == 0,
            )
        });
        match press.key {
            Key::Character('f' | 'F') if primary && !press.modifiers.shift => {
                self.focus_prefix();
                Some(TabAction::None)
            }
            Key::Character('/')
                if !primary && matches!(self.focus, TabFocus::Grid | TabFocus::Editor) =>
            {
                self.focus_prefix();
                Some(TabAction::None)
            }
            Key::PageDown if press.modifiers.alt => Some(self.next_page()),
            Key::PageUp if press.modifiers.alt => Some(self.previous_page()),
            Key::PageDown if in_grid && on_last_row && self.key_pages.can_go_next() => {
                Some(self.next_page())
            }
            Key::PageUp if in_grid && on_first_row && self.key_pages.can_go_back() => {
                Some(self.previous_page())
            }
            Key::Escape if self.running && in_grid => Some(TabAction::Stop),
            _ => None,
        }
    }

    fn prefix_key(&mut self, press: KeyPress, now: Instant) -> TabAction {
        let primary = press.modifiers.control || press.modifiers.command;
        let pages = &mut self.key_pages;
        match press.key {
            Key::Enter => {
                pages.due = None;
                self.leave_prefix();
                return self.first_page();
            }
            Key::Escape => {
                let changed = pages.prefix_text != pages.prefix_on_focus;
                pages.prefix_text = pages.prefix_on_focus.clone();
                pages.due = None;
                self.leave_prefix();
                return if changed {
                    self.first_page()
                } else {
                    TabAction::None
                };
            }
            Key::Tab => {
                self.leave_prefix();
                if press.modifiers.shift {
                    self.focus(TabFocus::Editor);
                }
                return TabAction::None;
            }
            Key::F5 => {
                pages.due = None;
                return self.first_page();
            }
            Key::Character('a' | 'A') if primary => pages.prefix_selected = true,
            Key::Character('f' | 'F') if primary => pages.prefix_selected = true,
            Key::Backspace => {
                if pages.prefix_selected {
                    pages.prefix_text.clear();
                } else if pages.prefix_text.pop().is_none() {
                    return TabAction::None;
                }
                pages.prefix_selected = false;
                pages.due = Some(now + PREFIX_DEBOUNCE);
            }
            Key::Character(letter) if !primary && !letter.is_control() => {
                if pages.prefix_selected {
                    pages.prefix_text.clear();
                }
                pages.prefix_text.push(letter);
                pages.prefix_selected = false;
                pages.due = Some(now + PREFIX_DEBOUNCE);
            }
            Key::Space if !primary => {
                if pages.prefix_selected {
                    pages.prefix_text.clear();
                }
                pages.prefix_text.push(' ');
                pages.prefix_selected = false;
                pages.due = Some(now + PREFIX_DEBOUNCE);
            }
            _ => {}
        }
        TabAction::None
    }

    /// Moves time on for a keys tab's prefix field: once 150 ms passed since the last key,
    /// page 1 runs with the typed prefix, unless it is invalid or a run is going.
    pub fn prefix_due(&mut self, now: Instant) -> TabAction {
        match self.key_pages.due {
            Some(due) if now >= due && !self.running => {
                self.key_pages.due = None;
                if self.key_pages.prefix_error().is_some() {
                    return TabAction::None;
                }
                self.first_page()
            }
            _ => TabAction::None,
        }
    }

    /// A keys tab's page was read (`docs/ux/workspace.md` 10.10): the tab takes the page's
    /// start, the pages before it and where the next starts, and its editor line states what
    /// ran.
    pub fn key_page_read(&mut self, request: &RunRequest, end: PageEnd) {
        let Some(scan) = &request.scan else {
            return;
        };
        let pending = self.key_pages.pending_earlier.take();
        self.key_pages.earlier = if scan.start == PageStart::First {
            Vec::new()
        } else {
            pending.unwrap_or_else(|| self.key_pages.earlier.clone())
        };
        let mut shown = scan.clone();
        if let Some(total) = end.total {
            shown.total = total;
        }
        shown.more = end.next.is_some();
        self.key_pages.next = end.next;
        self.editor = Editor::new(&shown.statement());
        self.keys = Some(shown);
    }

    /// A run of `request` started: `Run` and `Run all` disable, `Stop` enables, the
    /// summary reads `Running · 0.0 s` and the error strip hides.
    pub fn started(&mut self) {
        self.running = true;
        self.stopping = false;
        self.summary = status_line::running_summary(Duration::ZERO);
        if let Some(error) = &mut self.error {
            error.strip_shown = false;
        }
    }

    /// Moves the running summary on to `elapsed`; returns whether its text changed.
    pub fn show_running(&mut self, elapsed: Duration) -> bool {
        if !self.running || self.stopping {
            return false;
        }
        let summary = status_line::running_summary(elapsed);
        let changed = summary != self.summary;
        self.summary = summary;
        changed
    }

    /// `Stop` was pressed: the summary reads `Stopping…` until the engine confirms.
    pub fn stop_requested(&mut self) {
        if self.running {
            self.stopping = true;
            self.summary = "Stopping…".to_owned();
        }
    }

    /// Takes a finished run's outcomes, one per statement that ran, the last the failure
    /// when one failed: the grid gets the last result set, `Messages` the tag lines and the
    /// engine's error text, and the summary, the status bar and the inline error follow
    /// `docs/ux/workspace.md` 2.4 and 2.5.
    pub fn finish(
        &mut self,
        request: &RunRequest,
        outcomes: Vec<Result<Vec<ResultTable>, StatementFailure>>,
        elapsed: Duration,
    ) {
        if let Some(explain) = &request.explain {
            self.finish_explain(explain, &request.source_text, outcomes, elapsed);
            return;
        }
        let kept_column = self.grid.as_ref().map_or(0, |grid| grid.current.column);
        let was_stopping = self.stopping;
        self.running = false;
        self.stopping = false;
        self.error = None;
        self.messages.clear();
        let ran = outcomes.len();
        let mut last_rows: Option<(ResultTable, usize)> = None;
        let mut last_tag = String::new();
        let mut failure = None;
        for (index, outcome) in outcomes.into_iter().enumerate() {
            match outcome {
                Ok(tables) => {
                    for table in tables {
                        self.messages.push(table.tag.clone());
                        last_tag = table.tag.clone();
                        if !table.columns.is_empty() {
                            last_rows = Some((table, index));
                        }
                    }
                }
                Err(statement_failure) if failure.is_none() => {
                    failure = Some((statement_failure, index));
                }
                Err(_) => {}
            }
        }
        match last_rows {
            Some((table, index)) => {
                self.show_result(table, request.statements.get(index).map(String::as_str));
            }
            None if failure.is_none() => {
                self.grid = None;
                if self.focus == TabFocus::Grid {
                    self.focus = TabFocus::Editor;
                }
            }
            None => {}
        }
        let shown_rows = self.grid.as_ref().map_or(0, |grid| grid.rows.len());
        if let Some((statement_failure, index)) = failure {
            self.messages
                .extend(statement_failure.report.iter().cloned());
            if statement_failure.code == CANCELLED && was_stopping {
                self.summary = status_line::stopped_summary(shown_rows);
                self.status_tags = "Stopped".to_owned();
                return;
            }
            self.summary = "Error".to_owned();
            self.status_tags = "Error".to_owned();
            self.view = ResultView::Messages;
            self.show_error(&statement_failure, request, index);
            return;
        }
        self.view = ResultView::Results;
        self.status_tags = status_line::tag_summary(&last_tag, elapsed);
        self.summary = match &self.grid {
            _ if request.all && ran > 1 => status_line::run_all_summary(ran, shown_rows, elapsed),
            Some(grid) if grid.truncated() => status_line::truncated_summary(
                grid.rows.len(),
                grid.row_count,
                elapsed,
                status_line::wire_bytes(&grid.rows),
            ),
            Some(grid) if last_tag_has_rows(&self.messages) => status_line::result_summary(
                grid.rows.len(),
                elapsed,
                status_line::wire_bytes(&grid.rows),
            ),
            _ => status_line::tag_summary(&last_tag, elapsed),
        };
        if request.scan.is_some()
            && let Some(scan) = self.keys.clone()
        {
            self.summary = scan.summary(shown_rows, elapsed);
            let land_on_last = std::mem::take(&mut self.key_pages.land_on_last);
            if let Some(grid) = &mut self.grid {
                grid.key_scan = Some(scan);
                grid.current.column = kept_column.min(grid.columns.len().saturating_sub(1));
                if land_on_last && !grid.rows.is_empty() {
                    grid.current.row = grid.rows.len() - 1;
                }
                grid.anchor = grid.current;
                grid.scroll_into_view(ROWS_IN_VIEW, crate::main_window::GRID_BODY_WIDTH);
            }
        }
    }

    fn finish_explain(
        &mut self,
        explain: &ExplainRun,
        source_text: &str,
        outcomes: Vec<Result<Vec<ResultTable>, StatementFailure>>,
        elapsed: Duration,
    ) {
        let was_stopping = self.stopping;
        self.running = false;
        self.stopping = false;
        self.error = None;
        self.messages.clear();
        let mut plan_table = None;
        let mut failure = None;
        for (index, outcome) in outcomes.into_iter().enumerate() {
            match outcome {
                Ok(tables) => {
                    for table in tables {
                        self.messages.push(table.tag.clone());
                        if index == explain.plan_at && !table.columns.is_empty() {
                            plan_table = Some(table);
                        }
                    }
                }
                Err(statement_failure) => failure = Some((statement_failure, index)),
            }
        }
        if let Some((statement_failure, index)) = failure {
            self.messages
                .extend(statement_failure.report.iter().cloned());
            if statement_failure.code == CANCELLED && was_stopping {
                self.summary = status_line::stopped_summary(0);
                self.status_tags = "Stopped".to_owned();
                return;
            }
            self.summary = "Error".to_owned();
            self.status_tags = "Error".to_owned();
            self.view = ResultView::Messages;
            if index == explain.statement_at {
                let mut shifted = statement_failure;
                shifted.position = shifted
                    .position
                    .and_then(|position| position.checked_sub(explain.prefix_len));
                let original = RunRequest {
                    statements: vec![explain.statement_text.clone()],
                    starts: vec![explain.statement.start],
                    dialect: self.dialect,
                    all: false,
                    explain: None,
                    source_text: source_text.to_owned(),
                    session: self.session,
                    scan: None,
                };
                self.show_error(&shifted, &original, 0);
            }
            return;
        }
        let read = plan_table
            .ok_or(crate::plan_view::PlanError::Empty)
            .and_then(|table| {
                let tag = table.tag.clone();
                Plan::from_result(&table).map(|plan| (plan, tag))
            });
        match read {
            Ok((plan, tag)) => {
                let nodes = plan.nodes.len();
                self.plan = Some(PlanView::new(
                    plan,
                    explain.statement.clone(),
                    explain.statement_text.clone(),
                ));
                if self.focus == TabFocus::Plan {
                    self.focus_plan();
                }
                self.view = ResultView::Plan;
                self.status_tags = status_line::tag_summary(&tag, elapsed);
                let noun = if nodes == 1 { "node" } else { "nodes" };
                self.summary = format!(
                    "{} {noun} · {}",
                    status_line::thousands(nodes),
                    status_line::elapsed_text(elapsed)
                );
            }
            Err(error) => {
                self.messages.push(error.to_string());
                self.summary = "Error".to_owned();
                self.status_tags = "Error".to_owned();
                self.view = ResultView::Messages;
            }
        }
    }

    fn show_result(&mut self, table: ResultTable, statement: Option<&str>) {
        let sample: Vec<Vec<String>> = table
            .rows
            .iter()
            .take(200)
            .map(|row| {
                row.iter()
                    .enumerate()
                    .map(|(column, value)| {
                        let kind = table.kinds.get(column).copied().unwrap_or_default();
                        cell_text(value.as_deref(), kind).0
                    })
                    .collect()
            })
            .collect();
        let widths = column_widths(&table.columns, &sample, &crate::grid::measured);
        let mut grid = ResultGrid::new(
            table.columns,
            table.kinds,
            table.rows,
            table.row_count,
            widths,
        );
        grid.dialect = self.dialect;
        grid.source_table = statement.and_then(|sql| source_table(sql, self.dialect));
        grid.focused = self.focus == TabFocus::Grid;
        self.value_view = None;
        self.grid_menu = None;
        if let Some(earlier) = &self.grid {
            grid.keep_widths_of(earlier);
        }
        self.grid = Some(grid);
    }

    fn show_error(&mut self, failure: &StatementFailure, request: &RunRequest, index: usize) {
        let text = request.source_text.as_str();
        let start = request.starts.get(index).copied().unwrap_or(0);
        let statement_len = request.statements.get(index).map_or(0, String::len);
        let range = match failure.position {
            Some(position) => {
                let from = char_boundary_at_or_before(text, start.saturating_add(position));
                from..token_end(text, from).max(from)
            }
            None => {
                char_boundary_at_or_before(text, start)
                    ..char_boundary_at_or_before(text, start.saturating_add(statement_len))
            }
        };
        let unchanged = self.editor.text() == text;
        let at = if unchanged {
            let at = self.editor.position_at(range.start);
            self.editor.place_caret(at);
            at
        } else {
            Editor::new(text).position_at(range.start)
        };
        self.error = Some(InlineError {
            code: failure.code.clone(),
            message: failure.message.clone(),
            range: unchanged.then_some(range),
            line: at.line + 1,
            column: at.column + 1,
            fix: failure.hint.clone(),
            strip_shown: true,
        });
    }
}

fn last_tag_has_rows(messages: &[String]) -> bool {
    messages.last().is_some_and(|tag| {
        let verb = tag.split(' ').next().unwrap_or("");
        matches!(
            verb,
            "SELECT" | "FETCH" | "SHOW" | "EXPLAIN" | "VALUES" | "TABLE"
        ) || tag.contains("RETURNING")
    })
}

fn char_boundary_at_or_before(text: &str, offset: usize) -> usize {
    let mut at = offset.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Where the token that starts at byte `from` of `text` ends: a word, a quoted name or one
/// character, so the squiggle covers it; `from` itself when it is past the end or inside a
/// character.
pub fn token_end(text: &str, from: usize) -> usize {
    let Some(rest) = text.get(from..) else {
        return from;
    };
    let mut characters = rest.char_indices();
    let Some((_, first)) = characters.next() else {
        return from;
    };
    if matches!(first, '"' | '`' | '[' | '\'') {
        let close = if first == '[' { ']' } else { first };
        return rest[first.len_utf8()..]
            .find(close)
            .map_or(text.len(), |end| {
                from + first.len_utf8() + end + close.len_utf8()
            });
    }
    if !(first.is_alphanumeric() || first == '_') {
        return from + first.len_utf8();
    }
    rest.char_indices()
        .find(|(_, character)| {
            !(character.is_alphanumeric() || *character == '_' || *character == '.')
        })
        .map_or(text.len(), |(end, _)| from + end)
}

/// The editor's key for a platform key press, `None` for keys the editor does not take.
pub fn editor_key(press: KeyPress) -> Option<editor::Key> {
    let primary = press.modifiers.control || press.modifiers.command;
    let select = press.modifiers.shift;
    Some(match press.key {
        Key::Character('a' | 'A') if primary => editor::Key::SelectAll,
        Key::Character('z' | 'Z') if primary => editor::Key::Undo,
        Key::Character(_) if primary => return None,
        Key::Character(character) => editor::Key::Char(character),
        Key::Space if !primary => editor::Key::Char(' '),
        Key::Enter if !primary => editor::Key::Enter,
        Key::Tab => editor::Key::Tab,
        Key::Backspace => editor::Key::Backspace,
        Key::Delete => editor::Key::Delete,
        Key::Left => editor::Key::Left { select },
        Key::Right => editor::Key::Right { select },
        Key::Up => editor::Key::Up { select },
        Key::Down => editor::Key::Down { select },
        Key::Home => editor::Key::Home,
        Key::End => editor::Key::End,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::ValueKind;
    use eggdb_platform::window::Modifiers;

    fn press(key: Key, shift: bool, control: bool) -> KeyPress {
        let mut press = KeyPress::plain(key);
        press.modifiers.shift = shift;
        press.modifiers.control = control;
        press
    }

    fn table(columns: &[&str], rows: Vec<Vec<Option<&str>>>, tag: &str) -> ResultTable {
        let rows: Vec<Vec<Option<String>>> = rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|value| value.map(str::to_owned))
                    .collect()
            })
            .collect();
        ResultTable {
            columns: columns.iter().map(|name| (*name).to_owned()).collect(),
            kinds: vec![ValueKind::Number; columns.len()],
            row_count: rows.len(),
            rows,
            tag: tag.to_owned(),
        }
    }

    #[test]
    fn ctrl_enter_runs_the_statement_under_the_caret_and_shift_runs_all() {
        let mut tab = QueryTab::with_text("Query 1", "SELECT 1;\nSELECT 2;");
        let TabAction::Run(one) = tab.key(press(Key::Enter, false, true)) else {
            panic!("Ctrl+Enter runs");
        };
        assert_eq!(one.statements, ["SELECT 2"]);
        assert_eq!(one.starts, [10]);
        let TabAction::Run(all) = tab.key(press(Key::Enter, true, true)) else {
            panic!("Ctrl+Shift+Enter runs all");
        };
        assert_eq!((all.statements.len(), all.all), (2, true));
        tab.editor.apply(editor::Key::Up { select: false });
        tab.editor.apply(editor::Key::Home);
        tab.editor.apply(editor::Key::Right { select: true });
        tab.editor.apply(editor::Key::Right { select: true });
        let selected = tab.run_request(false).unwrap();
        assert_eq!(selected.statements, ["SE"]);
        tab.started();
        assert_eq!(tab.key(press(Key::Enter, false, true)), TabAction::None);
        assert_eq!(tab.notice.as_deref(), Some(STILL_RUNNING));
        assert_eq!(tab.key(press(Key::Escape, false, false)), TabAction::Stop);
        assert_eq!(QueryTab::new("Query 2").run_request(false), None);
    }

    #[test]
    fn the_dialect_menu_switches_the_dialect_and_keeps_the_text() {
        let mut tab = QueryTab::with_text("Query 1", "SELECT `a` LIMIT 1");
        tab.focus = TabFocus::DialectPill;
        tab.key(press(Key::Space, false, false));
        assert_eq!(tab.dialect_menu, Some(0));
        tab.key(press(Key::Down, false, false));
        tab.key(press(Key::Enter, false, false));
        assert_eq!((tab.dialect, tab.dialect_menu), (QueryDialect::MySql, None));
        assert_eq!(tab.editor.text(), "SELECT `a` LIMIT 1");
        tab.key(press(Key::Enter, false, false));
        tab.key(press(Key::Character('o'), false, false));
        assert_eq!(tab.dialect_menu, Some(3));
        tab.key(press(Key::Escape, false, false));
        assert_eq!((tab.dialect, tab.dialect_menu), (QueryDialect::MySql, None));
    }

    #[test]
    fn a_finished_run_fills_the_grid_the_summary_and_the_messages() {
        let mut tab = QueryTab::with_text("Query 1", "SELECT id FROM customers");
        let request = tab.run_request(false).unwrap();
        tab.started();
        assert_eq!(tab.summary, "Running · 0.0 s");
        assert!(tab.show_running(Duration::from_millis(1200)));
        assert_eq!(tab.summary, "Running · 1.2 s");
        let rows = vec![vec![Some("1")], vec![Some("2")], vec![None]];
        tab.finish(
            &request,
            vec![Ok(vec![table(&["id"], rows, "SELECT 3")])],
            Duration::from_micros(400),
        );
        assert_eq!(tab.summary, "3 rows · 0.4 ms · 35 B");
        assert_eq!(tab.status_tags, "SELECT 3 · 0.4 ms");
        assert_eq!(tab.messages, ["SELECT 3"]);
        let grid = tab.grid.as_ref().unwrap();
        assert_eq!(grid.source_table.as_deref(), Some("customers"));
        assert_eq!(grid.widths, vec![56.0]);
        tab.finish(
            &request,
            vec![Ok(vec![table(&[], vec![], "INSERT 0 2")])],
            Duration::from_micros(300),
        );
        assert_eq!(tab.summary, "INSERT 0 2 · 0.3 ms");
        let all = RunRequest {
            all: true,
            ..request.clone()
        };
        tab.finish(
            &all,
            vec![
                Ok(vec![table(&[], vec![], "CREATE TABLE")]),
                Ok(vec![table(&["a"], vec![vec![Some("1")]], "SELECT 1")]),
            ],
            Duration::from_millis(12),
        );
        assert_eq!(tab.summary, "2 statements · 1 row · 12 ms");
        assert_eq!(tab.messages, ["CREATE TABLE", "SELECT 1"]);
    }

    #[test]
    fn an_error_after_the_text_changed_during_the_run_shows_no_squiggle_and_does_not_panic() {
        let mut tab = QueryTab::with_text("Query 1", "SELECT * FROM users;");
        let request = tab.run_request(false).unwrap();
        tab.started();
        tab.editor
            .place_caret(editor::Position { line: 0, column: 0 });
        tab.editor.paste("ééééééé\u{1F600}");
        let failure = StatementFailure {
            code: "42P01".into(),
            message: "relation \"users\" does not exist".into(),
            hint: None,
            position: Some(14),
            report: Vec::new(),
        };
        tab.finish(&request, vec![Err(failure)], Duration::from_millis(1));
        let error = tab.error.clone().unwrap();
        assert_eq!(error.range, None);
        assert_eq!((error.line, error.column), (1, 15));
        assert_eq!(tab.editor.text(), "ééééééé\u{1F600}SELECT * FROM users;");
    }

    #[test]
    fn token_end_inside_a_character_or_past_the_end_stays_put() {
        assert_eq!(token_end("é", 1), 1);
        assert_eq!(token_end("ab", 9), 9);
        assert_eq!(char_boundary_at_or_before("aé", 2), 1);
    }

    #[test]
    fn an_error_puts_the_squiggle_the_strip_and_the_engines_lines_up() {
        let mut tab = QueryTab::with_text("Query 1", "SELECT 1;\nSELECT * FROM users;");
        let request = tab.run_request(false).unwrap();
        let failure = StatementFailure {
            code: "42P01".into(),
            message: "relation \"users\" does not exist".into(),
            hint: None,
            position: Some(14),
            report: vec![
                "ERROR:  relation \"users\" does not exist".into(),
                "SQLSTATE:  42P01".into(),
            ],
        };
        tab.started();
        tab.finish(&request, vec![Err(failure)], Duration::from_millis(1));
        let error = tab.error.clone().unwrap();
        assert_eq!(error.range, Some(24..29));
        assert_eq!(
            error.strip_text(),
            "Error 42P01: relation \"users\" does not exist · Ln 2, Col 15"
        );
        assert_eq!(error.fix_label(), None);
        assert_eq!(
            (tab.summary.as_str(), tab.view),
            ("Error", ResultView::Messages)
        );
        assert_eq!(tab.messages.len(), 2);
        assert_eq!(
            tab.editor.caret(),
            editor::Position {
                line: 1,
                column: 14
            }
        );
        tab.key(press(Key::Escape, false, false));
        assert!(!tab.error.as_ref().unwrap().strip_shown);
        tab.key(press(Key::Character('x'), false, false));
        assert_eq!(tab.error, None, "editing the error's line clears it");
        let fix = InlineError {
            fix: Some("Try: CREATE TABLE users (id int) and run it again".into()),
            ..error
        };
        assert_eq!(
            fix.fix_label().as_deref(),
            Some("Fix: CREATE TABLE users (id int) and run it a")
        );
    }

    #[test]
    fn a_stopped_run_keeps_the_rows_and_says_partial() {
        let mut tab = QueryTab::with_text("Query 1", "SELECT pg_sleep(10)");
        let request = tab.run_request(false).unwrap();
        tab.started();
        tab.stop_requested();
        assert_eq!(tab.summary, "Stopping…");
        let cancelled = StatementFailure {
            code: CANCELLED.into(),
            message: "canceling statement due to user request".into(),
            hint: None,
            position: None,
            report: Vec::new(),
        };
        tab.finish(&request, vec![Err(cancelled)], Duration::from_millis(500));
        assert_eq!(tab.summary, "Stopped · 0 rows (partial)");
        assert!(!tab.running);
    }

    #[test]
    fn the_grid_takes_the_keys_after_ctrl_3_and_ctrl_c_copies() {
        let mut tab = QueryTab::with_text("Query 1", "SELECT id FROM t");
        let request = tab.run_request(false).unwrap();
        tab.finish(
            &request,
            vec![Ok(vec![table(
                &["id"],
                vec![vec![Some("1")], vec![Some("2")]],
                "SELECT 2",
            )])],
            Duration::ZERO,
        );
        tab.key(press(Key::Character('3'), false, true));
        assert_eq!(tab.focus, TabFocus::Grid);
        tab.key(press(Key::Down, true, false));
        let TabAction::Copy { text, toast } = tab.key(press(Key::Character('c'), false, true))
        else {
            panic!("Ctrl+C copies");
        };
        assert_eq!(
            (text.as_str(), toast.as_str()),
            ("1\n2\n", "Copied 2 cells")
        );
        assert_eq!(token_end("users WHERE", 0), 5);
        assert_eq!(token_end("\"Order Lines\" x", 0), 13);
        assert_eq!(token_end("(", 0), 1);
        assert_eq!(token_end("", 0), 0);
    }

    #[test]
    fn ctrl_e_explains_the_statement_and_the_plan_takes_the_results() {
        let mut tab = QueryTab::with_text(
            "Query 1",
            "SELECT 1;\nSELECT * FROM customers WHERE id = 2;",
        );
        let TabAction::Run(request) = tab.key(press(Key::Character('e'), false, true)) else {
            panic!("Ctrl+E runs EXPLAIN");
        };
        assert_eq!(
            request.statements,
            ["EXPLAIN SELECT * FROM customers WHERE id = 2"]
        );
        let explain = request.explain.clone().expect("an explain run");
        assert_eq!((explain.analyze, explain.statement.start), (false, 10));
        tab.started();
        let plan = table(
            &["QUERY PLAN"],
            vec![vec![Some("Index Scan using customers_pkey on customers")]],
            "EXPLAIN",
        );
        tab.finish(&request, vec![Ok(vec![plan])], Duration::from_micros(400));
        assert_eq!(tab.view, ResultView::Plan);
        assert_eq!(tab.summary, "1 node · 0.4 ms");
        assert!(tab.grid.is_none(), "Results keeps the previous rows");
        let _ = tab.key(press(Key::Character('3'), false, true));
        assert_eq!(tab.focus, TabFocus::Plan);
        let _ = tab.key(press(Key::Enter, false, false));
        let highlight = tab.plan.as_ref().and_then(|plan| plan.highlight.clone());
        let at = tab.editor.text().find("customers").unwrap();
        assert_eq!(highlight, Some(at..at + "customers".len()));
        let _ = tab.key(KeyPress {
            key: Key::Character('t'),
            modifiers: Modifiers {
                alt: true,
                ..Modifiers::default()
            },
        });
        assert!(tab.plan.as_ref().is_some_and(|plan| plan.text_shown));
    }

    #[test]
    fn ctrl_shift_e_wraps_a_write_in_a_rolled_back_transaction_and_oracle_says_not_supported() {
        let mut tab = QueryTab::with_text("Query 1", "DELETE FROM t WHERE id = 1");
        let TabAction::Run(request) = tab.key(press(Key::Character('E'), true, true)) else {
            panic!("Ctrl+Shift+E runs EXPLAIN ANALYZE");
        };
        assert_eq!(
            request.statements,
            ["EXPLAIN (ANALYZE, BUFFERS) DELETE FROM t WHERE id = 1"]
        );
        assert!(
            request
                .explain
                .as_ref()
                .is_some_and(|explain| explain.rolled_back)
        );
        let failure = StatementFailure {
            code: "42P01".to_owned(),
            message: "relation \"t\" does not exist".to_owned(),
            hint: None,
            position: Some(27 + 12),
            report: vec!["ERROR:  relation \"t\" does not exist".to_owned()],
        };
        tab.started();
        tab.finish(
            &request,
            vec![Ok(Vec::new()), Err(failure)],
            Duration::from_millis(1),
        );
        assert_eq!(tab.view, ResultView::Messages);
        let error = tab
            .error
            .as_ref()
            .expect("the squiggle sits under the table's name");
        assert_eq!(error.range, Some(12..13));
        let mut oracle = QueryTab::with_text("Query 1", "SELECT 1 FROM dual");
        oracle.set_dialect(QueryDialect::Oracle);
        assert_eq!(
            oracle.key(press(Key::Character('E'), true, true)),
            TabAction::None
        );
        assert_eq!(oracle.view, ResultView::Messages);
        assert!(
            oracle.messages[0].starts_with("not_supported: cannot explain (analyze) in Oracle's"),
            "{:?}",
            oracle.messages
        );
        let TabAction::Run(request) = oracle.key(press(Key::Character('e'), false, true)) else {
            panic!("Ctrl+E explains in Oracle's dialect");
        };
        assert_eq!(
            request.statements,
            [
                "EXPLAIN PLAN FOR SELECT 1 FROM dual",
                "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY)"
            ]
        );
        let explain = request.explain.clone().expect("an explain run");
        assert!(explain.rolled_back);
        assert_eq!((explain.statement_at, explain.plan_at), (1, 2));
        let mut sqlserver = QueryTab::with_text("Query 1", "SELECT 1");
        sqlserver.set_dialect(QueryDialect::SqlServer);
        let TabAction::Run(request) = sqlserver.key(press(Key::Character('e'), false, true)) else {
            panic!("Ctrl+E explains in SQL Server's dialect");
        };
        let explain = request.explain.clone().expect("an explain run");
        assert_eq!(request.statements, ["SELECT 1"]);
        assert_eq!(
            (explain.before, explain.after, explain.rolled_back),
            (
                vec!["SET SHOWPLAN_ALL ON".to_owned()],
                vec!["SET SHOWPLAN_ALL OFF".to_owned()],
                false
            )
        );
    }

    #[test]
    fn the_editor_copies_cuts_and_asks_for_a_paste() {
        let mut tab = QueryTab::with_text("Query 1", "SELECT 1;");
        assert_eq!(
            tab.key(press(Key::Character('c'), false, true)),
            TabAction::None
        );
        tab.key(press(Key::Left, true, false));
        tab.key(press(Key::Left, true, false));
        let TabAction::Copy { text, toast } = tab.key(press(Key::Character('c'), false, true))
        else {
            panic!("a selection copies");
        };
        assert_eq!(
            (text.as_str(), toast.as_str()),
            ("1;", "Copied 2 characters")
        );
        assert!(matches!(
            tab.key(press(Key::Character('x'), false, true)),
            TabAction::Copy { .. }
        ));
        assert_eq!(tab.editor.text(), "SELECT ");
        assert_eq!(
            tab.key(press(Key::Character('v'), false, true)),
            TabAction::Paste
        );
        tab.paste("2;\nSELECT 3;");
        assert_eq!(tab.editor.text(), "SELECT 2;\nSELECT 3;");
    }
}
