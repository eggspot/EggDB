use std::ops::Range;

use eggdb_platform::window::{Key, KeyPress};

use crate::dialect::QueryDialect;
use crate::highlight::is_keyword;

const MIN_COLUMN: f32 = 48.0;
const MAX_COLUMN: f32 = 480.0;
const SAMPLED_ROWS: usize = 200;
const CELL_PADDING: f32 = 12.0;
const BLOB_BYTES_SHOWN: usize = 16;
const VECTOR_VALUES_SHOWN: usize = 3;

/// The height of the grid's header row, in logical pixels.
pub const HEADER_HEIGHT: f32 = 28.0;
/// The height of a body row.
pub const ROW_HEIGHT: f32 = 24.0;
/// The width of the row-number gutter.
pub const GUTTER_WIDTH: f32 = 48.0;

/// Each column's width (`docs/ux/gui.md`, "Grid"): the wider of its header and its first 200
/// cells, measured by `width`, plus padding on both sides, between 48 and 480 pixels.
pub fn column_widths(
    headers: &[String],
    rows: &[Vec<String>],
    width: &dyn Fn(&str) -> f32,
) -> Vec<f32> {
    headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            let widest = rows
                .iter()
                .take(SAMPLED_ROWS)
                .filter_map(|row| row.get(column))
                .map(|cell| width(cell))
                .fold(width(header), f32::max);
            (widest + 2.0 * CELL_PADDING)
                .clamp(MIN_COLUMN, MAX_COLUMN)
                .ceil()
        })
        .collect()
}

/// The rows a grid `height` pixels tall shows from `first`, at 24 pixels a row.
pub fn visible_rows(total: usize, first: usize, height: f32) -> Range<usize> {
    let fits = (height / ROW_HEIGHT).floor().max(0.0) as usize;
    let start = first.min(total);
    start..start.saturating_add(fits).min(total)
}

/// The columns whose x range meets a viewport `width` pixels wide scrolled `scroll_x` pixels
/// to the right, for columns `widths` wide laid side by side from 0.
pub fn visible_columns(widths: &[f32], scroll_x: f32, width: f32) -> Range<usize> {
    let right = scroll_x + width;
    let mut left = 0.0;
    let mut first = None;
    let mut end = 0;
    for (index, column) in widths.iter().enumerate() {
        let column_right = left + column;
        if column_right > scroll_x && left < right {
            first.get_or_insert(index);
            end = index + 1;
        }
        left = column_right;
    }
    first.map_or(0..0, |first| first..end)
}

/// What a column holds, which sets how its cells are drawn and aligned
/// (`docs/ux/workspace.md` 3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValueKind {
    /// Integers, decimals, floats and money: right-aligned.
    Number,
    /// Text, dates, times, intervals and UUIDs.
    #[default]
    Text,
    /// `true` or `false`.
    Boolean,
    /// JSON and arrays, drawn compact.
    Json,
    /// Bytes, drawn as `0x` and the first 16 in hex.
    Blob,
    /// A vector, drawn as its first three values and its dimension.
    Vector,
    /// A key's time to live, right-aligned.
    Ttl,
}

impl ValueKind {
    /// Whether its cells align right.
    pub fn right_aligned(self) -> bool {
        matches!(self, Self::Number | Self::Ttl)
    }
}

/// How a cell's text is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellLook {
    /// `textPrimary`.
    Primary,
    /// `textSecondary`: blobs and vectors.
    Secondary,
    /// `textDisabled`: the word `NULL`.
    Null,
}

fn printable(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\u{0}'..='\u{1f}' => char::from_u32(0x2400 + character as u32).unwrap_or('·'),
            '\u{7f}' => '\u{2421}',
            other => other,
        })
        .collect()
}

fn blob_text(text: &str) -> String {
    let hex = text
        .strip_prefix("\\x")
        .or_else(|| text.strip_prefix("0x"))
        .unwrap_or(text);
    let shown: String = hex.chars().take(BLOB_BYTES_SHOWN * 2).collect();
    let more = hex.chars().count() > BLOB_BYTES_SHOWN * 2;
    format!("0x{shown}{}", if more { "…" } else { "" })
}

fn vector_text(text: &str) -> String {
    let inner = text.trim().trim_start_matches('[').trim_end_matches(']');
    let values: Vec<&str> = inner.split(',').map(str::trim).filter(|value| !value.is_empty()).collect();
    if values.len() <= VECTOR_VALUES_SHOWN {
        return format!("[{}]", values.join(", "));
    }
    format!(
        "[{}, … {}]",
        values[..VECTOR_VALUES_SHOWN].join(", "),
        values.len()
    )
}

/// A key's time to live in seconds as the grid shows it: `12 s`, `3 m 20 s`, `none`.
pub fn ttl_text(text: &str) -> String {
    let Ok(seconds) = text.trim().parse::<i64>() else {
        return text.to_owned();
    };
    if seconds < 0 {
        return "none".to_owned();
    }
    let (hours, minutes, seconds) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds} s"),
        (0, _) => format!("{minutes} m {seconds} s"),
        _ => format!("{hours} h {minutes} m {seconds} s"),
    }
}

/// The text a cell draws for `value` of `kind`, and how: `NULL` for a null, blobs and
/// vectors shortened, control characters as their visible symbols.
pub fn cell_text(value: Option<&str>, kind: ValueKind) -> (String, CellLook) {
    let Some(text) = value else {
        return ("NULL".to_owned(), CellLook::Null);
    };
    match kind {
        ValueKind::Blob => (blob_text(text), CellLook::Secondary),
        ValueKind::Vector => (vector_text(text), CellLook::Secondary),
        ValueKind::Ttl => (ttl_text(text), CellLook::Primary),
        ValueKind::Number | ValueKind::Text | ValueKind::Boolean | ValueKind::Json => {
            (printable(text), CellLook::Primary)
        }
    }
}

/// One cell of a result: its row and column, from 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell {
    /// The row.
    pub row: usize,
    /// The column.
    pub column: usize,
}

/// What a key did to the grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridAnswer {
    /// The key is not the grid's.
    Ignored,
    /// The current cell or the selection changed, or nothing moved at an edge.
    Moved,
    /// `Tab`: the focus moves on to the next region.
    LeaveForward,
    /// `Shift+Tab`: the focus moves back to the previous region.
    LeaveBack,
    /// `Enter` or `F2`: open the current cell's full value.
    OpenValue,
    /// A copy: the text for the clipboard and the toast that says what was copied.
    Copy {
        /// The copied text.
        text: String,
        /// The toast, such as `Copied 12 cells`.
        toast: String,
    },
}

/// A result in the grid: its columns, the rows fetched, the current cell, the selection's
/// anchor and the scroll position (`docs/ux/workspace.md` 3).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResultGrid {
    /// The column names.
    pub columns: Vec<String>,
    /// Each column's kind of value.
    pub kinds: Vec<ValueKind>,
    /// The fetched rows, `None` for NULL.
    pub rows: Vec<Vec<Option<String>>>,
    /// How many rows the result has, fetched or not.
    pub row_count: usize,
    /// Each column's width in logical pixels.
    pub widths: Vec<f32>,
    /// The first row in view.
    pub first_row: usize,
    /// How far the columns are scrolled to the right, in logical pixels.
    pub scroll_x: f32,
    /// The current cell.
    pub current: Cell,
    /// The selection's other corner.
    pub anchor: Cell,
    /// Whether the grid has the keyboard focus.
    pub focused: bool,
    /// The table the query read, when it read one: what `Copy as INSERT` names.
    pub source_table: Option<String>,
    /// The tab's dialect, for `Copy as INSERT`'s quoting.
    pub dialect: QueryDialect,
}

fn primary(press: &KeyPress) -> bool {
    press.modifiers.control || press.modifiers.command
}

impl ResultGrid {
    /// A grid on a result, columns `widths` wide, the current cell at row 1, column 1.
    pub fn new(
        columns: Vec<String>,
        kinds: Vec<ValueKind>,
        rows: Vec<Vec<Option<String>>>,
        row_count: usize,
        widths: Vec<f32>,
    ) -> Self {
        Self {
            columns,
            kinds,
            row_count: row_count.max(rows.len()),
            rows,
            widths,
            ..Self::default()
        }
    }

    /// Whether the result has more rows than were fetched into the grid.
    pub fn truncated(&self) -> bool {
        self.row_count > self.rows.len()
    }

    /// The kind of column `column`.
    pub fn kind(&self, column: usize) -> ValueKind {
        self.kinds.get(column).copied().unwrap_or_default()
    }

    /// The value at `cell`, `None` for NULL or a cell outside the fetched rows.
    pub fn value(&self, cell: Cell) -> Option<&str> {
        self.rows.get(cell.row)?.get(cell.column)?.as_deref()
    }

    /// The selection: the rows and columns between the anchor and the current cell.
    pub fn selection(&self) -> (Range<usize>, Range<usize>) {
        let rows = self.anchor.row.min(self.current.row)..self.anchor.row.max(self.current.row) + 1;
        let columns =
            self.anchor.column.min(self.current.column)..self.anchor.column.max(self.current.column) + 1;
        (rows, columns)
    }

    /// Whether `cell` is in the selection.
    pub fn is_selected(&self, cell: Cell) -> bool {
        let (rows, columns) = self.selection();
        rows.contains(&cell.row) && columns.contains(&cell.column)
    }

    /// The selection's size in rows and columns.
    pub fn selection_size(&self) -> (usize, usize) {
        let (rows, columns) = self.selection();
        (rows.len(), columns.len())
    }

    fn last_row(&self) -> usize {
        self.rows.len().saturating_sub(1)
    }

    fn last_column(&self) -> usize {
        self.columns.len().saturating_sub(1)
    }

    fn move_to(&mut self, to: Cell, extend: bool) {
        self.current = Cell {
            row: to.row.min(self.last_row()),
            column: to.column.min(self.last_column()),
        };
        if !extend {
            self.anchor = self.current;
        }
    }

    /// The column edges, from 0, without the gutter: column `i` spans `edges[i]..edges[i + 1]`.
    pub fn column_edges(&self) -> Vec<f32> {
        let mut edges = Vec::with_capacity(self.widths.len() + 1);
        let mut left = 0.0;
        edges.push(left);
        for width in &self.widths {
            left += width;
            edges.push(left);
        }
        edges
    }

    /// Scrolls so the current cell is in a body `rows_in_view` rows tall and `width` pixels
    /// wide.
    pub fn scroll_into_view(&mut self, rows_in_view: usize, width: f32) {
        let rows_in_view = rows_in_view.max(1);
        if self.current.row < self.first_row {
            self.first_row = self.current.row;
        } else if self.current.row >= self.first_row + rows_in_view {
            self.first_row = self.current.row + 1 - rows_in_view;
        }
        let edges = self.column_edges();
        if let (Some(&left), Some(&right)) = (
            edges.get(self.current.column),
            edges.get(self.current.column + 1),
        ) {
            if left < self.scroll_x {
                self.scroll_x = left;
            } else if right > self.scroll_x + width {
                self.scroll_x = (right - width).max(0.0).min(left);
            }
        }
    }

    /// Answers a key while the grid has the focus (`docs/ux/workspace.md` 3.3 to 3.5), the
    /// body showing `rows_in_view` rows.
    pub fn key(&mut self, press: KeyPress, rows_in_view: usize) -> GridAnswer {
        let shift = press.modifiers.shift;
        let primary = primary(&press);
        let here = self.current;
        let page = rows_in_view.max(1);
        let to = match press.key {
            Key::Tab if shift => return GridAnswer::LeaveBack,
            Key::Tab => return GridAnswer::LeaveForward,
            Key::Enter | Key::F2 => return GridAnswer::OpenValue,
            Key::Escape => {
                self.anchor = self.current;
                return GridAnswer::Moved;
            }
            Key::Character('c' | 'C') if primary => return self.copy_tsv(shift),
            Key::Character('i' | 'I') if primary && shift => return self.copy_inserts(),
            Key::Character('a' | 'A') if primary => {
                self.anchor = Cell::default();
                self.current = Cell {
                    row: self.last_row(),
                    column: self.last_column(),
                };
                return GridAnswer::Moved;
            }
            Key::Space if shift => {
                self.anchor = Cell { row: here.row, column: 0 };
                self.current = Cell { row: here.row, column: self.last_column() };
                return GridAnswer::Moved;
            }
            Key::Space if primary => {
                self.anchor = Cell { row: 0, column: here.column };
                self.current = Cell { row: self.last_row(), column: here.column };
                return GridAnswer::Moved;
            }
            Key::Left => Cell { column: here.column.saturating_sub(1), ..here },
            Key::Right => Cell { column: here.column + 1, ..here },
            Key::Up => Cell { row: here.row.saturating_sub(1), ..here },
            Key::Down => Cell { row: here.row + 1, ..here },
            Key::PageUp => Cell { row: here.row.saturating_sub(page), ..here },
            Key::PageDown => Cell { row: here.row.saturating_add(page), ..here },
            Key::Home if primary => Cell { row: 0, ..here },
            Key::End if primary => Cell { row: self.last_row(), ..here },
            Key::Home => Cell { column: 0, ..here },
            Key::End => Cell { column: self.last_column(), ..here },
            _ => return GridAnswer::Ignored,
        };
        if self.rows.is_empty() || self.columns.is_empty() {
            return GridAnswer::Moved;
        }
        self.move_to(to, shift);
        GridAnswer::Moved
    }

    /// The selection as TSV (`docs/ux/workspace.md` 3.5): cells joined by tabs, each row
    /// ended by `\n`, NULL as nothing, tabs, newlines, returns and backslashes escaped; with
    /// `header`, the column names first.
    pub fn tsv(&self, header: bool) -> String {
        let (rows, columns) = self.selection();
        let mut out = String::new();
        if header {
            let names: Vec<String> = columns
                .clone()
                .filter_map(|column| self.columns.get(column))
                .map(|name| tsv_field(name))
                .collect();
            out.push_str(&names.join("\t"));
            out.push('\n');
        }
        for row in rows.filter(|&row| row < self.rows.len()) {
            let cells: Vec<String> = columns
                .clone()
                .map(|column| self.value(Cell { row, column }).map(tsv_field).unwrap_or_default())
                .collect();
            out.push_str(&cells.join("\t"));
            out.push('\n');
        }
        out
    }

    fn copy_tsv(&self, header: bool) -> GridAnswer {
        let (rows, columns) = self.selection_size();
        let rows = rows.min(self.rows.len().saturating_sub(self.selection().0.start));
        GridAnswer::Copy {
            text: self.tsv(header),
            toast: crate::status_line::copied_cells_text(rows * columns),
        }
    }

    /// The selected rows as one `INSERT` statement each in the grid's dialect, with every
    /// selected column (`docs/ux/workspace.md` 3.5).
    pub fn insert_statements(&self) -> String {
        let (rows, columns) = self.selection();
        let table = self.source_table.clone().unwrap_or_else(|| "result".to_owned());
        let names: Vec<String> = columns
            .clone()
            .filter_map(|column| self.columns.get(column))
            .map(|name| quote_name(name, self.dialect))
            .collect();
        let mut out = String::new();
        for row in rows.filter(|&row| row < self.rows.len()) {
            let values: Vec<String> = columns
                .clone()
                .map(|column| {
                    sql_literal(self.value(Cell { row, column }), self.kind(column), self.dialect)
                })
                .collect();
            out.push_str(&format!(
                "INSERT INTO {table} ({}) VALUES ({});\n",
                names.join(", "),
                values.join(", ")
            ));
        }
        out
    }

    fn copy_inserts(&self) -> GridAnswer {
        let text = self.insert_statements();
        let statements = text.lines().count();
        GridAnswer::Copy {
            text,
            toast: crate::status_line::copied_inserts_text(statements),
        }
    }
}

fn tsv_field(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out
}

fn plain_name(name: &str, dialect: QueryDialect) -> bool {
    let mut characters = name.chars();
    let starts = characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_');
    let folded = match dialect {
        QueryDialect::PostgreSql => !name.chars().any(|c| c.is_ascii_uppercase()),
        QueryDialect::Oracle => !name.chars().any(|c| c.is_ascii_lowercase()),
        QueryDialect::MySql | QueryDialect::SqlServer | QueryDialect::Sqlite => true,
    };
    starts
        && folded
        && characters.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !is_keyword(name, dialect)
}

/// `name` as an identifier in `dialect`: bare when it can be, else quoted the dialect's way
/// (`"Order"`, `` `order` ``, `[order]`).
pub fn quote_name(name: &str, dialect: QueryDialect) -> String {
    if plain_name(name, dialect) {
        return name.to_owned();
    }
    match dialect {
        QueryDialect::MySql => format!("`{}`", name.replace('`', "``")),
        QueryDialect::SqlServer => format!("[{}]", name.replace(']', "]]")),
        QueryDialect::PostgreSql | QueryDialect::Oracle | QueryDialect::Sqlite => {
            format!("\"{}\"", name.replace('"', "\"\""))
        }
    }
}

fn numeric_literal(text: &str) -> bool {
    let body = text.strip_prefix('-').unwrap_or(text);
    !body.is_empty()
        && body.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        && body.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '.')
        && text.parse::<f64>().is_ok()
}

/// `value` of `kind` as a literal in `dialect`: `NULL`, numbers bare, booleans the dialect's
/// way, blobs in its hex form, everything else a string quoted and escaped the dialect's way.
pub fn sql_literal(value: Option<&str>, kind: ValueKind, dialect: QueryDialect) -> String {
    let Some(text) = value else {
        return "NULL".to_owned();
    };
    match kind {
        ValueKind::Number | ValueKind::Ttl if numeric_literal(text) => text.to_owned(),
        ValueKind::Boolean => {
            let truth = matches!(text, "t" | "true" | "TRUE" | "1");
            match (dialect, truth) {
                (QueryDialect::SqlServer | QueryDialect::Oracle | QueryDialect::Sqlite, true) => "1".to_owned(),
                (QueryDialect::SqlServer | QueryDialect::Oracle | QueryDialect::Sqlite, false) => "0".to_owned(),
                (_, true) => "TRUE".to_owned(),
                (_, false) => "FALSE".to_owned(),
            }
        }
        ValueKind::Blob => {
            let hex = text.strip_prefix("\\x").or_else(|| text.strip_prefix("0x")).unwrap_or(text);
            if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return string_literal(text, dialect);
            }
            match dialect {
                QueryDialect::PostgreSql => format!("'\\x{hex}'"),
                QueryDialect::MySql | QueryDialect::Sqlite => format!("X'{hex}'"),
                QueryDialect::SqlServer => format!("0x{hex}"),
                QueryDialect::Oracle => format!("HEXTORAW('{hex}')"),
            }
        }
        _ => string_literal(text, dialect),
    }
}

fn string_literal(text: &str, dialect: QueryDialect) -> String {
    let doubled = text.replace('\'', "''");
    match dialect {
        QueryDialect::MySql => format!("'{}'", doubled.replace('\\', "\\\\")),
        QueryDialect::SqlServer if !text.is_ascii() => format!("N'{doubled}'"),
        _ => format!("'{doubled}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_fit_their_widest_sampled_cell_within_the_limits() {
        let measure = |text: &str| text.chars().count() as f32 * 8.0;
        let headers = vec!["id".to_owned(), "name".to_owned(), "notes".to_owned()];
        let rows = vec![
            vec!["1".into(), "Ada Lovelace".into(), "x".repeat(200)],
            vec!["2".into(), "Grace".into(), String::new()],
        ];
        assert_eq!(
            column_widths(&headers, &rows, &measure),
            vec![48.0, 120.0, 480.0]
        );
        assert_eq!(visible_rows(10, 8, 100.0), 8..10);
        assert_eq!(visible_rows(10, 20, 100.0), 10..10);
    }

    fn press(key: Key, shift: bool, control: bool) -> KeyPress {
        let mut press = KeyPress::plain(key);
        press.modifiers.shift = shift;
        press.modifiers.control = control;
        press
    }

    fn customers() -> ResultGrid {
        let rows = vec![
            vec![Some("1".into()), Some("Ada".into()), None],
            vec![Some("2".into()), Some("Gr\tace".into()), Some("O'Hara".into())],
            vec![Some("3".into()), Some("Alan".into()), Some("x".into())],
        ];
        let mut grid = ResultGrid::new(
            vec!["id".into(), "name".into(), "note".into()],
            vec![ValueKind::Number, ValueKind::Text, ValueKind::Text],
            rows,
            3,
            vec![80.0, 240.0, 160.0],
        );
        grid.source_table = Some("customers".into());
        grid
    }

    #[test]
    fn only_the_columns_meeting_the_viewport_are_visible() {
        let widths = [100.0, 200.0, 50.0, 300.0];
        assert_eq!(visible_columns(&widths, 0.0, 250.0), 0..2);
        assert_eq!(visible_columns(&widths, 100.0, 200.0), 1..2);
        assert_eq!(visible_columns(&widths, 120.0, 200.0), 1..3);
        assert_eq!(visible_columns(&widths, 1000.0, 200.0), 0..0);
        assert_eq!(visible_columns(&[], 0.0, 200.0), 0..0);
        assert_eq!(visible_rows(1_000_000, 999_990, 330.0), 999_990..1_000_000);
        assert_eq!(visible_rows(1_000_000, 0, 330.0), 0..13);
        assert_eq!(visible_rows(5, 0, -10.0), 0..0);
    }

    #[test]
    fn arrows_move_shift_extends_and_escape_collapses() {
        let mut grid = customers();
        grid.key(press(Key::Right, false, false), 10);
        grid.key(press(Key::Down, false, false), 10);
        grid.key(press(Key::Right, true, false), 10);
        grid.key(press(Key::Down, true, false), 10);
        assert_eq!(grid.current, Cell { row: 2, column: 2 });
        assert_eq!(grid.selection_size(), (2, 2));
        assert_eq!(grid.key(press(Key::Right, false, false), 10), GridAnswer::Moved);
        assert_eq!(grid.current, Cell { row: 2, column: 2 }, "no wrap at the edge");
        assert_eq!(grid.key(press(Key::Escape, false, false), 10), GridAnswer::Moved);
        assert_eq!(grid.selection_size(), (1, 1));
        assert_eq!(grid.key(press(Key::Tab, false, false), 10), GridAnswer::LeaveForward);
        assert_eq!(grid.key(press(Key::Tab, true, false), 10), GridAnswer::LeaveBack);
        grid.key(press(Key::Home, false, true), 10);
        assert_eq!(grid.current, Cell { row: 0, column: 2 });
        grid.key(press(Key::End, false, true), 10);
        assert_eq!(grid.current.row, 2);
        grid.key(press(Key::Home, false, false), 10);
        assert_eq!(grid.current.column, 0);
        grid.key(press(Key::PageUp, false, false), 2);
        assert_eq!(grid.current.row, 0);
        grid.key(press(Key::Space, true, false), 10);
        assert_eq!(grid.selection(), (0..1, 0..3));
        grid.key(press(Key::Space, false, true), 10);
        assert_eq!(grid.selection(), (0..3, 0..1));
        grid.key(press(Key::Character('a'), false, true), 10);
        assert_eq!(grid.selection(), (0..3, 0..3));
        assert_eq!(grid.key(press(Key::F2, false, false), 10), GridAnswer::OpenValue);
        assert_eq!(grid.key(press(Key::Character('x'), false, false), 10), GridAnswer::Ignored);
    }

    #[test]
    fn copy_makes_tsv_and_inserts_with_the_dialects_quoting() {
        let mut grid = customers();
        grid.anchor = Cell { row: 0, column: 0 };
        grid.current = Cell { row: 1, column: 1 };
        assert_eq!(grid.tsv(false), "1\tAda\n2\tGr\\tace\n");
        assert_eq!(grid.tsv(true), "id\tname\n1\tAda\n2\tGr\\tace\n");
        let GridAnswer::Copy { text, toast } = grid.key(press(Key::Character('c'), false, true), 10) else {
            panic!("Ctrl+C copies");
        };
        assert_eq!((text.as_str(), toast.as_str()), ("1\tAda\n2\tGr\\tace\n", "Copied 4 cells"));
        grid.current = Cell { row: 1, column: 2 };
        assert_eq!(grid.tsv(false), "1\tAda\t\n2\tGr\\tace\tO'Hara\n");
        assert_eq!(
            grid.insert_statements(),
            "INSERT INTO customers (id, name, note) VALUES (1, 'Ada', NULL);\nINSERT INTO customers (id, name, note) VALUES (2, 'Gr\tace', 'O''Hara');\n"
        );
        let GridAnswer::Copy { toast, .. } = grid.key(press(Key::Character('I'), true, true), 10) else {
            panic!("Ctrl+Shift+I copies INSERTs");
        };
        assert_eq!(toast, "Copied 2 INSERT statements");
        grid.dialect = QueryDialect::MySql;
        grid.source_table = None;
        grid.anchor = Cell { row: 1, column: 2 };
        assert_eq!(grid.insert_statements(), "INSERT INTO result (note) VALUES ('O''Hara');\n");
        assert_eq!(tsv_field("a\\b\r"), "a\\\\b\\r");
    }

    #[test]
    fn literals_and_names_follow_each_dialect() {
        assert_eq!(sql_literal(Some("it's"), ValueKind::Text, QueryDialect::PostgreSql), "'it''s'");
        assert_eq!(sql_literal(Some("a\\b"), ValueKind::Text, QueryDialect::MySql), "'a\\\\b'");
        assert_eq!(sql_literal(Some("é"), ValueKind::Text, QueryDialect::SqlServer), "N'é'");
        assert_eq!(sql_literal(Some("-1.5e3"), ValueKind::Number, QueryDialect::PostgreSql), "-1.5e3");
        assert_eq!(sql_literal(Some("NaN"), ValueKind::Number, QueryDialect::PostgreSql), "'NaN'");
        assert_eq!(sql_literal(Some("t"), ValueKind::Boolean, QueryDialect::PostgreSql), "TRUE");
        assert_eq!(sql_literal(Some("false"), ValueKind::Boolean, QueryDialect::SqlServer), "0");
        assert_eq!(sql_literal(Some("\\x0aff"), ValueKind::Blob, QueryDialect::Sqlite), "X'0aff'");
        assert_eq!(sql_literal(Some("\\x0aff"), ValueKind::Blob, QueryDialect::SqlServer), "0x0aff");
        assert_eq!(sql_literal(Some("2024-01-02"), ValueKind::Text, QueryDialect::Oracle), "'2024-01-02'");
        assert_eq!(sql_literal(None, ValueKind::Text, QueryDialect::Oracle), "NULL");
        assert_eq!(quote_name("name", QueryDialect::PostgreSql), "name");
        assert_eq!(quote_name("Name", QueryDialect::PostgreSql), "\"Name\"");
        assert_eq!(quote_name("order", QueryDialect::MySql), "`order`");
        assert_eq!(quote_name("a b", QueryDialect::SqlServer), "[a b]");
        assert_eq!(quote_name("ID", QueryDialect::Oracle), "ID");
        assert_eq!(quote_name("say\"hi", QueryDialect::Sqlite), "\"say\"\"hi\"");
    }

    #[test]
    fn cells_draw_each_kind_as_the_spec_says() {
        assert_eq!(cell_text(None, ValueKind::Number), ("NULL".to_owned(), CellLook::Null));
        assert_eq!(cell_text(Some("42"), ValueKind::Number), ("42".to_owned(), CellLook::Primary));
        assert_eq!(cell_text(Some("a\tb"), ValueKind::Text).0, "a\u{2409}b");
        let twenty = format!("\\x{}", "ab".repeat(20));
        assert_eq!(cell_text(Some(&twenty), ValueKind::Blob), (format!("0x{}…", "ab".repeat(16)), CellLook::Secondary));
        assert_eq!(cell_text(Some("\\x0102"), ValueKind::Blob).0, "0x0102");
        let vector = format!("[{}]", vec!["0.5"; 768].join(","));
        assert_eq!(cell_text(Some(&vector), ValueKind::Vector).0, "[0.5, 0.5, 0.5, … 768]");
        assert_eq!(cell_text(Some("[1,2]"), ValueKind::Vector).0, "[1, 2]");
        assert_eq!(cell_text(Some("12"), ValueKind::Ttl).0, "12 s");
        assert_eq!(cell_text(Some("200"), ValueKind::Ttl).0, "3 m 20 s");
        assert_eq!(cell_text(Some("-1"), ValueKind::Ttl).0, "none");
        assert!(ValueKind::Ttl.right_aligned() && ValueKind::Number.right_aligned());
        assert!(!ValueKind::Boolean.right_aligned());
    }

    #[test]
    fn the_current_cell_scrolls_into_view() {
        let mut grid = customers();
        grid.current = Cell { row: 2, column: 2 };
        grid.scroll_into_view(2, 200.0);
        assert_eq!(grid.first_row, 1);
        assert_eq!(grid.scroll_x, 280.0);
        grid.current = Cell { row: 0, column: 0 };
        grid.scroll_into_view(2, 200.0);
        assert_eq!((grid.first_row, grid.scroll_x), (0, 0.0));
    }
}
