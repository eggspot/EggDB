use std::ops::Range;
use std::time::Duration;

use eggdb_platform::window::{Key, KeyPress};

use crate::connection::{ResultTable, StatementFailure};
use crate::dialect::QueryDialect;
use crate::editor::{self, Editor};
use crate::grid::{GridAnswer, ResultGrid, cell_text, column_widths};
use crate::statements::{source_table, statement_at, statements};
use crate::status_line;

const FIX_LABEL_CHARACTERS: usize = 40;
const CANCELLED: &str = "57014";
const TEXT_ADVANCE: f32 = 8.0;

/// The rows the results body shows at the golden frame's size, for paging keys.
pub const ROWS_IN_VIEW: usize = 13;
/// What the status bar says when a run key is pressed while a statement runs.
pub const STILL_RUNNING: &str = "Running… press Esc to stop";

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
}

/// The results header's segment that is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResultView {
    /// The grid.
    #[default]
    Results,
    /// The statements' tag lines and the engine's error text.
    Messages,
}

/// An error from the engine shown in the editor (`docs/ux/workspace.md` 2.5): the squiggle
/// under its range and the strip at the editor's bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineError {
    /// The SQLSTATE, such as `42P01`.
    pub code: String,
    /// The engine's message.
    pub message: String,
    /// The bytes of the editor's text the squiggle is under.
    pub range: Range<usize>,
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
        let ranges: Vec<Range<usize>> = if all {
            statements(&text, self.dialect)
        } else if let Some(selected) = self.editor.selected_bytes() {
            statements(&text[selected.clone()], self.dialect)
                .into_iter()
                .map(|range| range.start + selected.start..range.end + selected.start)
                .collect()
        } else {
            let caret = self.editor.offset(self.editor.caret());
            statement_at(&text, caret, self.dialect).into_iter().collect()
        };
        if ranges.is_empty() {
            return None;
        }
        Some(RunRequest {
            statements: ranges.iter().map(|range| text[range.clone()].to_owned()).collect(),
            starts: ranges.iter().map(|range| range.start).collect(),
            dialect: self.dialect,
            all,
        })
    }

    /// Answers a key (`docs/ux/workspace.md` 2.2, 2.3, 3.3 to 3.5).
    pub fn key(&mut self, press: KeyPress) -> TabAction {
        let primary = press.modifiers.control || press.modifiers.command;
        if let Some(highlighted) = self.dialect_menu {
            self.menu_key(press, highlighted);
            return TabAction::None;
        }
        let run_all = primary && press.modifiers.shift;
        match press.key {
            Key::Enter if primary => return self.run_key(run_all),
            Key::F5 => return self.run_key(false),
            Key::Character('.') if press.modifiers.command && self.running => {
                return TabAction::Stop;
            }
            Key::Escape if self.running && self.focus == TabFocus::Editor => return TabAction::Stop,
            Key::Character('3') if primary => {
                if self.grid.is_some() {
                    self.focus_grid();
                }
                return TabAction::None;
            }
            _ => {}
        }
        match self.focus {
            TabFocus::Editor => self.editor_key(press),
            TabFocus::DialectPill => self.pill_key(press),
            TabFocus::Grid => self.grid_key(press),
        }
    }

    fn run_key(&mut self, all: bool) -> TabAction {
        if self.running {
            self.notice = Some(STILL_RUNNING.to_owned());
            return TabAction::None;
        }
        self.run_request(all).map_or(TabAction::None, TabAction::Run)
    }

    fn focus_grid(&mut self) {
        self.focus = TabFocus::Grid;
        self.view = ResultView::Results;
        if let Some(grid) = &mut self.grid {
            grid.focused = true;
        }
    }

    fn focus(&mut self, focus: TabFocus) {
        if focus == TabFocus::Grid && self.grid.is_some() {
            self.focus_grid();
            return;
        }
        self.focus = if focus == TabFocus::Grid { TabFocus::Editor } else { focus };
        if let Some(grid) = &mut self.grid {
            grid.focused = false;
        }
    }

    fn menu_key(&mut self, press: KeyPress, highlighted: usize) {
        let last = QueryDialect::ALL.len() - 1;
        match press.key {
            Key::Up => self.dialect_menu = Some(highlighted.saturating_sub(1)),
            Key::Down => self.dialect_menu = Some((highlighted + 1).min(last)),
            Key::Home => self.dialect_menu = Some(0),
            Key::End => self.dialect_menu = Some(last),
            Key::Enter | Key::Space => {
                self.set_dialect(QueryDialect::at(highlighted));
                self.dialect_menu = None;
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

    fn pill_key(&mut self, press: KeyPress) -> TabAction {
        match press.key {
            Key::Space | Key::Enter => self.dialect_menu = Some(self.dialect.index()),
            Key::Down if press.modifiers.alt => self.dialect_menu = Some(self.dialect.index()),
            Key::Tab if press.modifiers.shift => self.focus(TabFocus::Grid),
            Key::Tab => self.focus(TabFocus::Editor),
            _ => {}
        }
        TabAction::None
    }

    fn grid_key(&mut self, press: KeyPress) -> TabAction {
        let Some(grid) = &mut self.grid else {
            self.focus(TabFocus::Editor);
            return TabAction::None;
        };
        match grid.key(press, ROWS_IN_VIEW) {
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
                grid.scroll_into_view(ROWS_IN_VIEW, crate::main_window::GRID_BODY_WIDTH);
                TabAction::None
            }
            GridAnswer::OpenValue | GridAnswer::Ignored => TabAction::None,
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
        let Some(key) = editor_key(press) else {
            return TabAction::None;
        };
        let before = self.editor.text();
        let line = self.editor.caret().line;
        self.editor.apply(key);
        if self.editor.text() != before
            && self.error.as_ref().is_some_and(|error| error.line == line + 1)
        {
            self.error = None;
        }
        TabAction::None
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
                Err(statement_failure) => failure = Some((statement_failure, index)),
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
            self.messages.extend(statement_failure.report.iter().cloned());
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
        let measure = |text: &str| text.chars().count() as f32 * TEXT_ADVANCE;
        let widths = column_widths(&table.columns, &sample, &measure);
        let mut grid = ResultGrid::new(table.columns, table.kinds, table.rows, table.row_count, widths);
        grid.dialect = self.dialect;
        grid.source_table = statement.and_then(|sql| source_table(sql, self.dialect));
        grid.focused = self.focus == TabFocus::Grid;
        self.grid = Some(grid);
    }

    fn show_error(&mut self, failure: &StatementFailure, request: &RunRequest, index: usize) {
        let text = self.editor.text();
        let start = request.starts.get(index).copied().unwrap_or(0);
        let statement_len = request.statements.get(index).map_or(0, String::len);
        let range = match failure.position {
            Some(position) => {
                let from = (start + position).min(text.len());
                from..token_end(&text, from).max(from)
            }
            None => start..(start + statement_len).min(text.len()),
        };
        let at = self.editor.position_at(range.start);
        self.editor.place_caret(at);
        self.error = Some(InlineError {
            code: failure.code.clone(),
            message: failure.message.clone(),
            range,
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
        matches!(verb, "SELECT" | "FETCH" | "SHOW" | "EXPLAIN" | "VALUES" | "TABLE") || tag.contains("RETURNING")
    })
}

/// Where the token that starts at byte `from` of `text` ends: a word, a quoted name or one
/// character, so the squiggle covers it.
pub fn token_end(text: &str, from: usize) -> usize {
    let rest = &text[from..];
    let mut characters = rest.char_indices();
    let Some((_, first)) = characters.next() else {
        return from;
    };
    if matches!(first, '"' | '`' | '[' | '\'') {
        let close = if first == '[' { ']' } else { first };
        return rest[first.len_utf8()..]
            .find(close)
            .map_or(text.len(), |end| from + first.len_utf8() + end + close.len_utf8());
    }
    if !(first.is_alphanumeric() || first == '_') {
        return from + first.len_utf8();
    }
    rest.char_indices()
        .find(|(_, character)| !(character.is_alphanumeric() || *character == '_' || *character == '.'))
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

    fn press(key: Key, shift: bool, control: bool) -> KeyPress {
        let mut press = KeyPress::plain(key);
        press.modifiers.shift = shift;
        press.modifiers.control = control;
        press
    }

    fn table(columns: &[&str], rows: Vec<Vec<Option<&str>>>, tag: &str) -> ResultTable {
        let rows: Vec<Vec<Option<String>>> = rows
            .into_iter()
            .map(|row| row.into_iter().map(|value| value.map(str::to_owned)).collect())
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
        tab.finish(&request, vec![Ok(vec![table(&["id"], rows, "SELECT 3")])], Duration::from_micros(400));
        assert_eq!(tab.summary, "3 rows · 0.4 ms · 35 B");
        assert_eq!(tab.status_tags, "SELECT 3 · 0.4 ms");
        assert_eq!(tab.messages, ["SELECT 3"]);
        let grid = tab.grid.as_ref().unwrap();
        assert_eq!(grid.source_table.as_deref(), Some("customers"));
        assert_eq!(grid.widths, vec![56.0]);
        tab.finish(&request, vec![Ok(vec![table(&[], vec![], "INSERT 0 2")])], Duration::from_micros(300));
        assert_eq!(tab.summary, "INSERT 0 2 · 0.3 ms");
        let all = RunRequest { all: true, ..request.clone() };
        tab.finish(
            &all,
            vec![Ok(vec![table(&[], vec![], "CREATE TABLE")]), Ok(vec![table(&["a"], vec![vec![Some("1")]], "SELECT 1")])],
            Duration::from_millis(12),
        );
        assert_eq!(tab.summary, "2 statements · 1 row · 12 ms");
        assert_eq!(tab.messages, ["CREATE TABLE", "SELECT 1"]);
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
            report: vec!["ERROR:  relation \"users\" does not exist".into(), "SQLSTATE:  42P01".into()],
        };
        tab.started();
        tab.finish(&request, vec![Err(failure)], Duration::from_millis(1));
        let error = tab.error.clone().unwrap();
        assert_eq!(error.range, 24..29);
        assert_eq!(error.strip_text(), "Error 42P01: relation \"users\" does not exist · Ln 2, Col 15");
        assert_eq!(error.fix_label(), None);
        assert_eq!((tab.summary.as_str(), tab.view), ("Error", ResultView::Messages));
        assert_eq!(tab.messages.len(), 2);
        assert_eq!(tab.editor.caret(), editor::Position { line: 1, column: 14 });
        tab.key(press(Key::Escape, false, false));
        assert!(!tab.error.as_ref().unwrap().strip_shown);
        tab.key(press(Key::Character('x'), false, false));
        assert_eq!(tab.error, None, "editing the error's line clears it");
        let fix = InlineError { fix: Some("Try: CREATE TABLE users (id int) and run it again".into()), ..error };
        assert_eq!(fix.fix_label().as_deref(), Some("Fix: CREATE TABLE users (id int) and run it a"));
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
        tab.finish(&request, vec![Ok(vec![table(&["id"], vec![vec![Some("1")], vec![Some("2")]], "SELECT 2")])], Duration::ZERO);
        tab.key(press(Key::Character('3'), false, true));
        assert_eq!(tab.focus, TabFocus::Grid);
        tab.key(press(Key::Down, true, false));
        let TabAction::Copy { text, toast } = tab.key(press(Key::Character('c'), false, true)) else {
            panic!("Ctrl+C copies");
        };
        assert_eq!((text.as_str(), toast.as_str()), ("1\n2\n", "Copied 2 cells"));
        assert_eq!(token_end("users WHERE", 0), 5);
        assert_eq!(token_end("\"Order Lines\" x", 0), 13);
        assert_eq!(token_end("(", 0), 1);
        assert_eq!(token_end("", 0), 0);
    }
}
