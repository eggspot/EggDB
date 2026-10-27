use super::{Hint, ToastKind, Ui};
use crate::editor::{Editor, Lines, Outcome};
use crate::frame::Frame;
use crate::source::{Source, SqlRun, SqlTable};
use crate::terminal::{KeyCode, KeyEvent};
use crate::text;
use crate::theme::{Role, Symbol};
use crate::width::text_width;
use std::time::Duration;

const MOST_HISTORY_RUNS: usize = 100;
const WIDEST_CELL: usize = 48;
const RUN_KEYS: &str = "F5 or Ctrl-R run";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryFocus {
    Editor,
    Grid,
}

#[derive(Debug)]
pub(super) struct Query {
    editor: Editor,
    focus: QueryFocus,
    run: Option<SqlRun>,
    top_row: usize,
    first_col: usize,
    history: Vec<String>,
    recalled: usize,
    draft: String,
}

impl Query {
    pub(super) fn new() -> Self {
        Self {
            editor: Editor::new(String::new(), Lines::Multi),
            focus: QueryFocus::Editor,
            run: None,
            top_row: 0,
            first_col: 0,
            history: Vec::new(),
            recalled: 0,
            draft: String::new(),
        }
    }
}

fn duration_text(elapsed: Duration) -> String {
    let millis = elapsed.as_secs_f64() * 1000.0;
    if millis < 1000.0 {
        format!("{millis:.1} ms")
    } else {
        format!("{:.2} s", millis / 1000.0)
    }
}

fn rows_text(count: usize) -> String {
    let unit = if count == 1 { "row" } else { "rows" };
    format!("{} {unit}", text::count(count as u64))
}

fn widths(table: &SqlTable) -> Vec<usize> {
    let mut widths: Vec<usize> = table.columns.iter().map(|name| text_width(name)).collect();
    for row in &table.rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.as_deref().map_or(0, text_width));
        }
    }
    widths
        .iter()
        .map(|width| (*width).min(WIDEST_CELL))
        .collect()
}

fn fitted(cell: &str, width: usize, right: bool, ellipsis: &str) -> String {
    let mut shown = String::new();
    let mut used = 0;
    let cut = text_width(cell) > width;
    let room = if cut { width.saturating_sub(1) } else { width };
    for character in cell.chars() {
        let character_width = text_width(character.encode_utf8(&mut [0u8; 4]));
        if used + character_width > room {
            break;
        }
        shown.push(character);
        used += character_width;
    }
    if cut {
        shown.push_str(ellipsis);
        used += 1;
    }
    let fill = " ".repeat(width.saturating_sub(used));
    if right {
        format!("{fill}{shown}")
    } else {
        format!("{shown}{fill}")
    }
}

fn centered(name: &str, width: usize, ellipsis: &str) -> String {
    let fitted_name = fitted(name, width, false, ellipsis);
    let trimmed = fitted_name.trim_end();
    let fill = width.saturating_sub(text_width(trimmed));
    let left = fill / 2;
    format!("{}{trimmed}{}", " ".repeat(left), " ".repeat(fill - left))
}

impl<S: Source> Ui<S> {
    pub(super) fn open_query(&mut self) {
        if self.query.is_none() {
            self.query = Some(Query::new());
        }
        self.query_open = true;
    }

    fn editor_rows(&self) -> usize {
        if self.size.rows >= 20 { 4 } else { 2 }
    }

    fn grid_rows(&self) -> usize {
        usize::from(self.size.rows).saturating_sub(self.editor_rows() + 5)
    }

    pub(super) fn query_paste(&mut self, pasted: &str) {
        if let Some(query) = self.query.as_mut()
            && query.focus == QueryFocus::Editor
        {
            query.editor.paste_as_text(pasted);
        }
        self.follow_query_cursor();
    }

    pub(super) fn query_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.ctrl;
        let run_key = key.code == KeyCode::F(5) || (ctrl && key.code == KeyCode::Char('r'));
        if run_key {
            self.run_query();
            return;
        }
        let Some(query) = self.query.as_mut() else {
            return;
        };
        match query.focus {
            QueryFocus::Editor => match key.code {
                KeyCode::Tab | KeyCode::BackTab => query.focus = QueryFocus::Grid,
                KeyCode::Esc => self.close_query(),
                KeyCode::F(1) => self.help = Some(0),
                KeyCode::Char('p') if ctrl => recall(query, -1),
                KeyCode::Char('n') if ctrl => recall(query, 1),
                _ => {
                    let page = self.editor_rows();
                    if let Some(query) = self.query.as_mut()
                        && query.editor.key(key, page) == Outcome::TooLong
                    {
                        self.info("The editor holds at most 4 MiB of text");
                    }
                }
            },
            QueryFocus::Grid => {
                if ctrl && key.code != KeyCode::Home && key.code != KeyCode::End {
                    return;
                }
                self.grid_key(key);
            }
        }
        self.follow_query_cursor();
    }

    fn grid_key(&mut self, key: KeyEvent) {
        let page = self.grid_rows().saturating_sub(2).max(1);
        let Some(query) = self.query.as_mut() else {
            return;
        };
        let (rows, cols) = query
            .run
            .as_ref()
            .and_then(|run| run.table.as_ref())
            .map_or((0, 0), |table| (table.rows.len(), table.columns.len()));
        let last_top = rows.saturating_sub(1);
        match key.code {
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc => query.focus = QueryFocus::Editor,
            KeyCode::Down | KeyCode::Char('j') => {
                query.top_row = (query.top_row + 1).min(last_top);
            }
            KeyCode::Up | KeyCode::Char('k') => query.top_row = query.top_row.saturating_sub(1),
            KeyCode::PageDown => query.top_row = (query.top_row + page).min(last_top),
            KeyCode::PageUp => query.top_row = query.top_row.saturating_sub(page),
            KeyCode::Home if key.modifiers.ctrl => query.first_col = 0,
            KeyCode::End if key.modifiers.ctrl => query.first_col = cols.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => query.top_row = 0,
            KeyCode::End | KeyCode::Char('G') => {
                query.top_row = rows.saturating_sub(page);
            }
            KeyCode::Right => {
                query.first_col = (query.first_col + 1).min(cols.saturating_sub(1));
            }
            KeyCode::Left => query.first_col = query.first_col.saturating_sub(1),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') | KeyCode::F(1) => self.help = Some(0),
            _ => {}
        }
    }

    fn close_query(&mut self) {
        self.query_open = false;
        self.summary = self.source.summary();
        self.reload();
    }

    fn run_query(&mut self) {
        let Some(query) = self.query.as_ref() else {
            return;
        };
        let text = query.editor.text().to_owned();
        if text.trim().is_empty() {
            self.info("Nothing to run");
            return;
        }
        let run = self.source.run_sql(&text);
        let Some(query) = self.query.as_mut() else {
            return;
        };
        if query.history.last() != Some(&text) {
            query.history.push(text);
            if query.history.len() > MOST_HISTORY_RUNS {
                query.history.remove(0);
            }
        }
        query.recalled = query.history.len();
        query.top_row = 0;
        query.first_col = 0;
        if let Some(position) = run.failure.as_ref().and_then(|failure| failure.position) {
            query.editor.move_to(position);
            query.focus = QueryFocus::Editor;
        }
        let failed = run.failure.is_some();
        let mut status = run.tags.join(" · ");
        if failed {
            if !status.is_empty() {
                status.push_str(" · ");
            }
            status.push_str("ERROR");
        }
        for notice in &run.notices {
            if !status.is_empty() {
                status.push_str(" · ");
            }
            status.push_str(notice);
        }
        query.run = Some(run);
        if !status.is_empty() {
            let kind = if failed {
                ToastKind::Error
            } else {
                ToastKind::Info
            };
            self.toast(kind, status);
        }
        self.follow_query_cursor();
    }

    fn follow_query_cursor(&mut self) {
        let rows = self.editor_rows();
        let cols = usize::from(self.size.cols.saturating_sub(2));
        if let Some(query) = self.query.as_mut() {
            query.editor.scroll_into_view(rows, cols);
        }
    }

    pub(super) fn query_hints(&self) -> Option<Vec<Hint>> {
        if !self.query_open {
            return None;
        }
        let query = self.query.as_ref()?;
        let up_down = self.theme.symbol(Symbol::UpDown);
        let left_right = self.theme.symbol(Symbol::LeftRight);
        Some(match query.focus {
            QueryFocus::Editor => vec![
                Hint::new("F5", "run"),
                Hint::new("Tab", "grid"),
                Hint::new("Ctrl-P/Ctrl-N", "history"),
                Hint::new("Esc", "back"),
                Hint::new("F1", "help"),
                Hint::new("Ctrl-C", "quit"),
            ],
            QueryFocus::Grid => vec![
                Hint::new(up_down, "scroll"),
                Hint::new(left_right, "columns"),
                Hint::new("Home/End", "ends"),
                Hint::new("Tab", "editor"),
                Hint::new("F5", "run"),
                Hint::new("Esc", "editor"),
                Hint::new("?", "help"),
                Hint::new("q", "quit"),
            ],
        })
    }

    pub(super) fn draw_query(&self, frame: &mut Frame) {
        let Some(query) = &self.query else {
            return;
        };
        let cols = self.size.cols;
        let editor_rows = self.editor_rows();
        let editor_focused = query.focus == QueryFocus::Editor;
        let title_role = |focused: bool| {
            if focused {
                Role::FocusedTitle
            } else {
                Role::Title
            }
        };
        let role = title_role(editor_focused);
        frame.fill(0, 1, cols, self.theme.style(role));
        let position = format!(
            " SQL  ln {} col {}",
            text::count(query.editor.cursor_line() as u64 + 1),
            text::count(query.editor.cursor_col() as u64 + 1)
        );
        self.put(frame, 0, 1, &position, role, cols);
        let dot = self.theme.symbol(Symbol::MiddleDot);
        let reminder = format!("{RUN_KEYS} {dot} Tab grid ");
        if text_width(&position) + text_width(&reminder) < usize::from(cols) {
            self.put_right(frame, cols, 1, &reminder, role);
        }
        for (index, (_, line)) in query.editor.visible_lines(editor_rows).enumerate() {
            let segments = [(text::shown_in_cells(line), Role::Plain)];
            let row = 2 + index as u16;
            self.draw_segments(
                frame,
                1,
                row,
                cols.saturating_sub(1),
                &segments,
                query.editor.pan(),
            );
        }
        if editor_focused {
            let row = query
                .editor
                .cursor_line()
                .saturating_sub(query.editor.top());
            let col = query
                .editor
                .cursor_cells()
                .saturating_sub(query.editor.pan());
            frame.cursor = Some((
                1u16.saturating_add(u16::try_from(col).unwrap_or(u16::MAX))
                    .min(cols.saturating_sub(1)),
                2 + u16::try_from(row).unwrap_or(0),
            ));
        }
        let result_row = 2 + editor_rows as u16;
        let role = title_role(!editor_focused);
        frame.fill(0, result_row, cols, self.theme.style(role));
        let grid_top = result_row + 1;
        let grid_rows = self.grid_rows();
        let Some(run) = &query.run else {
            self.put(frame, 0, result_row, " Result", role, cols);
            self.draw_query_status(frame, None);
            return;
        };
        if let Some(failure) = &run.failure {
            self.put(frame, 0, result_row, " Result  ERROR", role, cols);
            for (index, line) in failure.lines.iter().take(grid_rows).enumerate() {
                let line_role = if index == 0 { Role::Error } else { Role::Plain };
                self.put(
                    frame,
                    1,
                    grid_top + index as u16,
                    line,
                    line_role,
                    cols.saturating_sub(1),
                );
            }
            self.draw_query_status(frame, Some(run));
            return;
        }
        let Some(table) = &run.table else {
            self.put(frame, 0, result_row, " Result  no rows to show", role, cols);
            self.draw_query_status(frame, Some(run));
            return;
        };
        let title = format!(
            " Result  {} {dot} {}",
            rows_text(table.rows.len()),
            duration_text(run.elapsed)
        );
        self.put(frame, 0, result_row, &title, role, cols);
        let body_rows = grid_rows.saturating_sub(2);
        let last_row = (query.top_row + body_rows).min(table.rows.len());
        let en_dash = self.theme.symbol(Symbol::EnDash);
        let widths = widths(table);
        let ellipsis = self.theme.symbol(Symbol::Ellipsis);
        let mut shown_cols = 0;
        let mut used = 0usize;
        for width in widths.iter().skip(query.first_col) {
            if shown_cols > 0 && used + width + 3 > usize::from(cols) {
                break;
            }
            used += width + 3;
            shown_cols += 1;
        }
        let window = format!(
            "rows {}{en_dash}{}/{} {dot} cols {}{en_dash}{}/{} ",
            text::count((query.top_row + 1).min(table.rows.len()) as u64),
            text::count(last_row as u64),
            text::count(table.rows.len() as u64),
            text::count((query.first_col + 1).min(table.columns.len()) as u64),
            text::count((query.first_col + shown_cols) as u64),
            text::count(table.columns.len() as u64),
        );
        if text_width(&title) + text_width(&window) < usize::from(cols) {
            self.put_right(frame, cols, result_row, &window, role);
        }
        let range = query.first_col..query.first_col + shown_cols;
        let header: Vec<String> = range
            .clone()
            .map(|index| centered(&table.columns[index], widths[index], ellipsis))
            .collect();
        let rule: Vec<String> = range
            .clone()
            .map(|index| "-".repeat(widths[index] + 2))
            .collect();
        let mut lines = vec![table_line(&header), rule.join("+")];
        for row in &table.rows[query.top_row.min(table.rows.len())..last_row] {
            let cells: Vec<String> = range
                .clone()
                .map(|index| {
                    let cell = row.get(index).cloned().flatten().unwrap_or_default();
                    let right = table.numeric.get(index).copied().unwrap_or(false);
                    fitted(&cell, widths[index], right, ellipsis)
                })
                .collect();
            lines.push(table_line(&cells));
        }
        for (index, line) in lines.iter().take(grid_rows).enumerate() {
            self.put(frame, 0, grid_top + index as u16, line, Role::Plain, cols);
        }
        if last_row == table.rows.len() && lines.len() < grid_rows {
            let count = format!("({})", rows_text(table.rows.len()));
            self.put(
                frame,
                0,
                grid_top + lines.len() as u16,
                &count,
                Role::Dim,
                cols,
            );
        }
        self.draw_query_status(frame, Some(run));
    }

    fn draw_query_status(&self, frame: &mut Frame, run: Option<&SqlRun>) {
        if self.toast.is_some() {
            return;
        }
        let Some(run) = run else {
            return;
        };
        let mut status = run.tags.join(" · ");
        if run.failure.is_some() {
            if !status.is_empty() {
                status.push_str(" · ");
            }
            status.push_str("ERROR");
        }
        let role = if run.failure.is_some() {
            Role::Error
        } else {
            Role::Plain
        };
        let row = self.size.rows.saturating_sub(2);
        self.put(
            frame,
            1,
            row,
            &status,
            role,
            self.size.cols.saturating_sub(2),
        );
    }
}

fn table_line(cells: &[String]) -> String {
    let joined: Vec<String> = cells.iter().map(|cell| format!(" {cell} ")).collect();
    joined.join("|").trim_end().to_owned()
}

fn recall(query: &mut Query, by: isize) {
    if query.history.is_empty() {
        return;
    }
    if query.recalled == query.history.len() {
        query.draft = query.editor.text().to_owned();
    }
    let mut target = query
        .recalled
        .saturating_add_signed(by)
        .min(query.history.len());
    if by < 0
        && query.recalled == query.history.len()
        && query.history.get(target).map(String::as_str) == Some(query.editor.text())
    {
        target = target.saturating_sub(1);
    }
    if target == query.recalled {
        return;
    }
    query.recalled = target;
    let text = query
        .history
        .get(target)
        .cloned()
        .unwrap_or_else(|| query.draft.clone());
    query.editor = Editor::new(text, Lines::Multi);
}
