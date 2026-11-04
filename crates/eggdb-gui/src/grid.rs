use std::ops::Range;

use eggdb_platform::window::{Key, KeyPress};

use crate::dialect::QueryDialect;
use crate::highlight::is_keyword;

const MIN_COLUMN: f32 = 48.0;
const MAX_COLUMN: f32 = 480.0;
const MAX_DRAGGED_COLUMN: f32 = 960.0;
const RESIZE_STEP: f32 = 8.0;
const RESIZE_BIG_STEP: f32 = 32.0;
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
/// The advance of one character of the grid's monospace text, which sizes columns.
pub const CHARACTER_ADVANCE: f32 = 8.0;

/// The width `text` takes at [`CHARACTER_ADVANCE`] a character.
pub fn measured(text: &str) -> f32 {
    text.chars().count() as f32 * CHARACTER_ADVANCE
}

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
    /// A key-value key or value spelled as text (`docs/ux/ui.md`), drawn as it is and an
    /// empty one as `(empty)`.
    SpelledBytes,
}

impl ValueKind {
    /// Whether its cells align right.
    pub fn right_aligned(self) -> bool {
        matches!(self, Self::Number | Self::Ttl)
    }

    /// The kind's name as the full-value view's header gives it: `text`, `number`, `binary`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Text => "text",
            Self::Boolean => "boolean",
            Self::Json => "json",
            Self::Blob => "binary",
            Self::Vector => "vector",
            Self::Ttl => "ttl",
            Self::SpelledBytes => "bytes",
        }
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
    let values: Vec<&str> = inner
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect();
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
        ValueKind::SpelledBytes if text.is_empty() => ("(empty)".to_owned(), CellLook::Null),
        ValueKind::SpelledBytes => (printable(text), CellLook::Primary),
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
    /// `Shift+F10` or the menu key: open the grid's menu below the current cell.
    OpenMenu,
    /// `Alt+Left`, `Alt+Right` or `Alt+Enter`: the current cell's column changed width.
    Resized,
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
    /// The key space a keys tab scanned, when the grid shows one (`docs/ux/workspace.md`
    /// 10.4).
    pub key_scan: Option<crate::key_scan::KeySpaceScan>,
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

    /// The banner's text over a cut result (`docs/ux/workspace.md` 3.7, 10.4).
    pub fn banner_text(&self) -> String {
        match &self.key_scan {
            Some(scan) => scan.banner(self.rows.len()),
            None => crate::status_line::truncated_banner(self.rows.len(), self.row_count),
        }
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
        let columns = self.anchor.column.min(self.current.column)
            ..self.anchor.column.max(self.current.column) + 1;
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

    /// Scrolls by `rows` rows and `columns` whole columns, as the wheel does
    /// (`docs/ux/workspace.md` 3.6), keeping the current cell and the selection; the first row
    /// stays within a body `rows_in_view` rows tall.
    pub fn scroll_by(&mut self, rows: isize, columns: isize, rows_in_view: usize) {
        let last_first = self.rows.len().saturating_sub(rows_in_view.max(1));
        self.first_row = self.first_row.saturating_add_signed(rows).min(last_first);
        if columns == 0 {
            return;
        }
        let edges = self.column_edges();
        let first_column = edges
            .iter()
            .rposition(|&edge| edge <= self.scroll_x)
            .unwrap_or(0);
        let last_column = self.widths.len().saturating_sub(1);
        let to = first_column.saturating_add_signed(columns).min(last_column);
        self.scroll_x = edges.get(to).copied().unwrap_or(0.0);
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
            Key::Enter if press.modifiers.alt => {
                self.auto_fit(here.column);
                return GridAnswer::Resized;
            }
            Key::Left | Key::Right if press.modifiers.alt => {
                let step = if shift { RESIZE_BIG_STEP } else { RESIZE_STEP };
                let step = if press.key == Key::Left { -step } else { step };
                let width = self.widths.get(here.column).copied().unwrap_or(MIN_COLUMN);
                self.set_width(here.column, width + step);
                return GridAnswer::Resized;
            }
            Key::Enter | Key::F2 => return GridAnswer::OpenValue,
            Key::F10 if shift => return GridAnswer::OpenMenu,
            Key::ContextMenu => return GridAnswer::OpenMenu,
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
                self.anchor = Cell {
                    row: here.row,
                    column: self.last_column(),
                };
                self.current = Cell {
                    row: here.row,
                    column: 0,
                };
                return GridAnswer::Moved;
            }
            Key::Space if primary => {
                self.anchor = Cell {
                    row: self.last_row(),
                    column: here.column,
                };
                self.current = Cell {
                    row: 0,
                    column: here.column,
                };
                return GridAnswer::Moved;
            }
            Key::Left => Cell {
                column: here.column.saturating_sub(1),
                ..here
            },
            Key::Right => Cell {
                column: here.column + 1,
                ..here
            },
            Key::Up => Cell {
                row: here.row.saturating_sub(1),
                ..here
            },
            Key::Down => Cell {
                row: here.row + 1,
                ..here
            },
            Key::PageUp => Cell {
                row: here.row.saturating_sub(page),
                ..here
            },
            Key::PageDown => Cell {
                row: here.row.saturating_add(page),
                ..here
            },
            Key::Home if primary => Cell { row: 0, ..here },
            Key::End if primary => Cell {
                row: self.last_row(),
                ..here
            },
            Key::Home => Cell { column: 0, ..here },
            Key::End => Cell {
                column: self.last_column(),
                ..here
            },
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
                .map(|column| {
                    self.value(Cell { row, column })
                        .map(tsv_field)
                        .unwrap_or_default()
                })
                .collect();
            out.push_str(&cells.join("\t"));
            out.push('\n');
        }
        out
    }

    /// Sets column `column`'s width, between 48 and 960 pixels (`docs/ux/workspace.md` 9.5);
    /// returns whether it changed.
    pub fn set_width(&mut self, column: usize, width: f32) -> bool {
        let Some(current) = self.widths.get_mut(column) else {
            return false;
        };
        let width = width.round().clamp(MIN_COLUMN, MAX_DRAGGED_COLUMN);
        let changed = *current != width;
        *current = width;
        changed
    }

    /// The width that fits column `column`: the wider of its header and its first 200
    /// fetched cells, between 48 and 480 pixels.
    pub fn fit_width(&self, column: usize) -> f32 {
        let Some(name) = self.columns.get(column) else {
            return MIN_COLUMN;
        };
        let kind = self.kind(column);
        let cells: Vec<Vec<String>> = self
            .rows
            .iter()
            .take(SAMPLED_ROWS)
            .map(|row| vec![cell_text(row.get(column).and_then(|value| value.as_deref()), kind).0])
            .collect();
        column_widths(std::slice::from_ref(name), &cells, &measured)
            .first()
            .copied()
            .unwrap_or(MIN_COLUMN)
    }

    /// Sets column `column` to the width that fits it; returns whether it changed.
    pub fn auto_fit(&mut self, column: usize) -> bool {
        let width = self.fit_width(column);
        self.set_width(column, width)
    }

    /// Keeps `earlier`'s widths when it had the same column names in the same order, as a
    /// tab's widths survive a run with the same columns (`docs/ux/workspace.md` 9.5).
    pub fn keep_widths_of(&mut self, earlier: &ResultGrid) {
        if earlier.columns == self.columns && earlier.widths.len() == self.widths.len() {
            self.widths.clone_from(&earlier.widths);
        }
    }

    /// Column `column`'s name and width as the status bar shows it while it is resized:
    /// `name · 240 px`.
    pub fn width_text(&self, column: usize) -> String {
        let name = self.columns.get(column).map_or("", String::as_str);
        let width = self.widths.get(column).copied().unwrap_or(0.0);
        format!("{name} · {width:.0} px")
    }

    /// The columns' total width.
    pub fn content_width(&self) -> f32 {
        self.widths.iter().sum()
    }

    fn fetched_selection(&self) -> (Range<usize>, Range<usize>) {
        let (rows, columns) = self.selection();
        let rows = rows.start.min(self.rows.len())..rows.end.min(self.rows.len());
        let columns = columns.start.min(self.columns.len())..columns.end.min(self.columns.len());
        (rows, columns)
    }

    /// The selection as RFC 4180 CSV (`docs/ux/workspace.md` 9.4): a header row of the
    /// selected columns' names, fields joined by `,` and records by `\r\n` with no trailing
    /// one, a field holding `,`, `"`, `\r` or `\n` quoted with `"` doubled, NULL empty and a
    /// blob as `0x` and its full hex.
    pub fn csv(&self) -> String {
        let (rows, columns) = self.fetched_selection();
        let mut records = Vec::with_capacity(rows.len() + 1);
        let header: Vec<String> = columns
            .clone()
            .map(|column| csv_field(&self.columns[column]))
            .collect();
        records.push(header.join(","));
        for row in rows {
            let fields: Vec<String> = columns
                .clone()
                .map(|column| match self.value(Cell { row, column }) {
                    None => String::new(),
                    Some(text) if self.kind(column) == ValueKind::Blob => {
                        csv_field(&full_hex(text))
                    }
                    Some(text) if self.kind(column) == ValueKind::Json && is_json(text) => {
                        csv_field(&compact_json(text))
                    }
                    Some(text) => csv_field(text),
                })
                .collect();
            records.push(fields.join(","));
        }
        records.join("\r\n")
    }

    /// The selection as a pretty-printed JSON array of one object per selected row
    /// (`docs/ux/workspace.md` 9.4): keys the column names in column order, a repeated name
    /// suffixed `_2`, `_3`; numbers and booleans bare, NULL `null`, JSON and vectors embedded,
    /// everything else a string.
    pub fn json(&self) -> String {
        let (rows, columns) = self.fetched_selection();
        let keys = json_keys(&self.columns[columns.clone()]);
        let mut objects = Vec::with_capacity(rows.len());
        for row in rows {
            let members: Vec<String> = columns
                .clone()
                .zip(&keys)
                .map(|(column, key)| {
                    let value = json_value(self.value(Cell { row, column }), self.kind(column));
                    format!("{}:{value}", json_string(key))
                })
                .collect();
            objects.push(format!("{{{}}}", members.join(",")));
        }
        pretty_json(&format!("[{}]", objects.join(",")))
    }

    fn copy_csv(&self) -> GridAnswer {
        let (rows, columns) = self.fetched_selection();
        GridAnswer::Copy {
            text: self.csv(),
            toast: crate::status_line::copied_csv_text(rows.len() * columns.len()),
        }
    }

    fn copy_json(&self) -> GridAnswer {
        let (rows, _) = self.fetched_selection();
        GridAnswer::Copy {
            text: self.json(),
            toast: crate::status_line::copied_json_text(rows.len()),
        }
    }

    /// Does what the grid's menu item `item` says (`docs/ux/workspace.md` 9.4).
    pub fn menu_item(&mut self, item: GridMenuItem) -> GridAnswer {
        let primary = |letter: char, shift: bool| KeyPress {
            key: Key::Character(letter),
            modifiers: eggdb_platform::window::Modifiers {
                control: true,
                shift,
                ..Default::default()
            },
        };
        let shift_space = KeyPress {
            key: Key::Space,
            modifiers: eggdb_platform::window::Modifiers {
                shift: true,
                ..Default::default()
            },
        };
        let control_space = KeyPress {
            key: Key::Space,
            modifiers: eggdb_platform::window::Modifiers {
                control: true,
                ..Default::default()
            },
        };
        let press = match item {
            GridMenuItem::CopyAsCsv => return self.copy_csv(),
            GridMenuItem::CopyAsJson => return self.copy_json(),
            GridMenuItem::OpenValue => return GridAnswer::OpenValue,
            GridMenuItem::Copy => primary('c', false),
            GridMenuItem::CopyWithHeader => primary('c', true),
            GridMenuItem::CopyAsInsert => primary('i', true),
            GridMenuItem::SelectRow => shift_space,
            GridMenuItem::SelectColumn => control_space,
            GridMenuItem::SelectAll => primary('a', false),
        };
        self.key(press, 1)
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
        let table = self
            .source_table
            .clone()
            .unwrap_or_else(|| "result".to_owned());
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
                    sql_literal(
                        self.value(Cell { row, column }),
                        self.kind(column),
                        self.dialect,
                    )
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
        let statements = self
            .selection()
            .0
            .filter(|&row| row < self.rows.len())
            .count();
        GridAnswer::Copy {
            text,
            toast: crate::status_line::copied_inserts_text(statements),
        }
    }
}

/// One item of the grid's right-click menu (`docs/ux/workspace.md` 9.4), in its order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridMenuItem {
    /// `Copy`: the selection as TSV.
    Copy,
    /// `Copy with header`: TSV with the column names first.
    CopyWithHeader,
    /// `Copy as INSERT`.
    CopyAsInsert,
    /// `Copy as CSV`.
    CopyAsCsv,
    /// `Copy as JSON`.
    CopyAsJson,
    /// `Open value`: the current cell's full-value view.
    OpenValue,
    /// `Select row`.
    SelectRow,
    /// `Select column`.
    SelectColumn,
    /// `Select all`.
    SelectAll,
}

impl GridMenuItem {
    /// Every item, top to bottom.
    pub const ALL: [Self; 9] = [
        Self::Copy,
        Self::CopyWithHeader,
        Self::CopyAsInsert,
        Self::CopyAsCsv,
        Self::CopyAsJson,
        Self::OpenValue,
        Self::SelectRow,
        Self::SelectColumn,
        Self::SelectAll,
    ];

    /// The item's label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Copy => "Copy",
            Self::CopyWithHeader => "Copy with header",
            Self::CopyAsInsert => "Copy as INSERT",
            Self::CopyAsCsv => "Copy as CSV",
            Self::CopyAsJson => "Copy as JSON",
            Self::OpenValue => "Open value",
            Self::SelectRow => "Select row",
            Self::SelectColumn => "Select column",
            Self::SelectAll => "Select all",
        }
    }

    /// The item's shortcut as the menu shows it, with `primary` naming `Ctrl` or `Cmd`.
    pub fn shortcut(self, primary: &str) -> Option<String> {
        match self {
            Self::Copy => Some(format!("{primary}+C")),
            Self::CopyWithHeader => Some(format!("{primary}+Shift+C")),
            Self::CopyAsInsert => Some(format!("{primary}+Shift+I")),
            Self::OpenValue => Some("Enter".to_owned()),
            Self::SelectRow => Some("Shift+Space".to_owned()),
            Self::SelectColumn => Some(format!("{primary}+Space")),
            Self::SelectAll => Some(format!("{primary}+A")),
            Self::CopyAsCsv | Self::CopyAsJson => None,
        }
    }

    /// Whether a separator follows the item, ending its group.
    pub fn ends_group(self) -> bool {
        matches!(self, Self::CopyAsJson | Self::OpenValue)
    }
}

/// A blob's text as `0x` and its full hex, from PostgreSQL's `\x` form or an `0x` one.
pub fn full_hex(text: &str) -> String {
    let hex = text
        .strip_prefix("\\x")
        .or_else(|| text.strip_prefix("0x"))
        .unwrap_or(text);
    format!("0x{hex}")
}

fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

fn json_keys(names: &[String]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        let mut key = name.clone();
        let mut suffix = 2;
        while keys.contains(&key) {
            key = format!("{name}_{suffix}");
            suffix += 1;
        }
        keys.push(key);
    }
    keys
}

/// `text` as a JSON string, quoted and escaped.
pub fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0}'..='\u{1f}' => out.push_str(&format!("\\u{:04x}", character as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn json_value(value: Option<&str>, kind: ValueKind) -> String {
    let Some(text) = value else {
        return "null".to_owned();
    };
    match kind {
        ValueKind::Number | ValueKind::Ttl if numeric_literal(text) && is_json(text) => {
            text.to_owned()
        }
        ValueKind::Boolean => {
            let truth = matches!(text, "t" | "true" | "TRUE" | "1");
            String::from(if truth { "true" } else { "false" })
        }
        ValueKind::Json | ValueKind::Vector if is_json(text) => text.trim().to_owned(),
        ValueKind::Blob => json_string(&full_hex(text)),
        _ => json_string(text),
    }
}

const JSON_DEPTH_LIMIT: usize = 256;

/// Whether `text` is one JSON value (RFC 8259) with nothing after it, nested at most 256
/// deep.
pub fn is_json(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut at = 0;
    json_element(bytes, &mut at, 0) && {
        skip_json_space(bytes, &mut at);
        at == bytes.len()
    }
}

fn skip_json_space(bytes: &[u8], at: &mut usize) {
    while bytes
        .get(*at)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
    {
        *at += 1;
    }
}

fn json_element(bytes: &[u8], at: &mut usize, depth: usize) -> bool {
    if depth > JSON_DEPTH_LIMIT {
        return false;
    }
    skip_json_space(bytes, at);
    match bytes.get(*at) {
        Some(b'{') => json_members(bytes, at, depth, b'}', true),
        Some(b'[') => json_members(bytes, at, depth, b']', false),
        Some(b'"') => json_string_token(bytes, at),
        Some(b't') => json_word(bytes, at, b"true"),
        Some(b'f') => json_word(bytes, at, b"false"),
        Some(b'n') => json_word(bytes, at, b"null"),
        Some(b'-' | b'0'..=b'9') => json_number(bytes, at),
        _ => false,
    }
}

fn json_members(bytes: &[u8], at: &mut usize, depth: usize, close: u8, object: bool) -> bool {
    *at += 1;
    skip_json_space(bytes, at);
    if bytes.get(*at) == Some(&close) {
        *at += 1;
        return true;
    }
    loop {
        if object {
            skip_json_space(bytes, at);
            if !json_string_token(bytes, at) {
                return false;
            }
            skip_json_space(bytes, at);
            if bytes.get(*at) != Some(&b':') {
                return false;
            }
            *at += 1;
        }
        if !json_element(bytes, at, depth + 1) {
            return false;
        }
        skip_json_space(bytes, at);
        match bytes.get(*at) {
            Some(b',') => *at += 1,
            Some(&byte) if byte == close => {
                *at += 1;
                return true;
            }
            _ => return false,
        }
    }
}

fn json_string_token(bytes: &[u8], at: &mut usize) -> bool {
    if bytes.get(*at) != Some(&b'"') {
        return false;
    }
    *at += 1;
    while let Some(&byte) = bytes.get(*at) {
        *at += 1;
        match byte {
            b'"' => return true,
            b'\\' => match bytes.get(*at) {
                Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => *at += 1,
                Some(b'u') => {
                    let digits = bytes.get(*at + 1..*at + 5);
                    if !digits.is_some_and(|digits| digits.iter().all(u8::is_ascii_hexdigit)) {
                        return false;
                    }
                    *at += 5;
                }
                _ => return false,
            },
            0..=0x1f => return false,
            _ => {}
        }
    }
    false
}

fn json_word(bytes: &[u8], at: &mut usize, word: &[u8]) -> bool {
    if bytes.get(*at..*at + word.len()) == Some(word) {
        *at += word.len();
        true
    } else {
        false
    }
}

fn json_digits(bytes: &[u8], at: &mut usize) -> usize {
    let start = *at;
    while bytes.get(*at).is_some_and(u8::is_ascii_digit) {
        *at += 1;
    }
    *at - start
}

fn json_number(bytes: &[u8], at: &mut usize) -> bool {
    if bytes.get(*at) == Some(&b'-') {
        *at += 1;
    }
    match bytes.get(*at) {
        Some(b'0') => *at += 1,
        Some(b'1'..=b'9') => {
            json_digits(bytes, at);
        }
        _ => return false,
    }
    if bytes.get(*at) == Some(&b'.') {
        *at += 1;
        if json_digits(bytes, at) == 0 {
            return false;
        }
    }
    if matches!(bytes.get(*at), Some(b'e' | b'E')) {
        *at += 1;
        if matches!(bytes.get(*at), Some(b'+' | b'-')) {
            *at += 1;
        }
        if json_digits(bytes, at) == 0 {
            return false;
        }
    }
    true
}

/// `text`, which [`is_json`] accepts, without the white space between its tokens.
pub fn compact_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for character in text.chars() {
        if in_string {
            out.push(character);
            match character {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => in_string = false,
                _ => escaped = false,
            }
            continue;
        }
        match character {
            ' ' | '\t' | '\n' | '\r' => {}
            '"' => {
                in_string = true;
                out.push('"');
            }
            other => out.push(other),
        }
    }
    out
}

/// `text` pretty-printed with a 2-space indent (`docs/ux/workspace.md` 9.3): a line per
/// member and element, `": "` after keys, empty objects and arrays kept as `{}` and `[]`,
/// strings untouched and keys in their stored order. Text that is not JSON (a PostgreSQL
/// array such as `{1,2}`) is laid out by the same brackets and commas.
pub fn pretty_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    let mut depth: usize = 0;
    let mut characters = text.chars().peekable();
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str("  ");
        }
    };
    while let Some(character) = characters.next() {
        match character {
            '"' => {
                out.push('"');
                let mut escaped = false;
                for inner in characters.by_ref() {
                    out.push(inner);
                    match inner {
                        '\\' if !escaped => escaped = true,
                        '"' if !escaped => break,
                        _ => escaped = false,
                    }
                }
            }
            '{' | '[' => {
                while characters.next_if(|next| next.is_whitespace()).is_some() {}
                let close = if character == '{' { '}' } else { ']' };
                if characters.next_if_eq(&close).is_some() {
                    out.push(character);
                    out.push(close);
                } else {
                    out.push(character);
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(character);
            }
            ',' => {
                out.push(',');
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            ' ' | '\t' | '\n' | '\r' => {}
            other => out.push(other),
        }
    }
    out
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
        && body
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        && body
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == '.')
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
                (QueryDialect::SqlServer | QueryDialect::Oracle | QueryDialect::Sqlite, true) => {
                    "1".to_owned()
                }
                (QueryDialect::SqlServer | QueryDialect::Oracle | QueryDialect::Sqlite, false) => {
                    "0".to_owned()
                }
                (_, true) => "TRUE".to_owned(),
                (_, false) => "FALSE".to_owned(),
            }
        }
        ValueKind::Blob => {
            let hex = text
                .strip_prefix("\\x")
                .or_else(|| text.strip_prefix("0x"))
                .unwrap_or(text);
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
            vec![
                Some("2".into()),
                Some("Gr\tace".into()),
                Some("O'Hara".into()),
            ],
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
        assert_eq!(
            grid.key(press(Key::Right, false, false), 10),
            GridAnswer::Moved
        );
        assert_eq!(
            grid.current,
            Cell { row: 2, column: 2 },
            "no wrap at the edge"
        );
        assert_eq!(
            grid.key(press(Key::Escape, false, false), 10),
            GridAnswer::Moved
        );
        assert_eq!(grid.selection_size(), (1, 1));
        assert_eq!(
            grid.key(press(Key::Tab, false, false), 10),
            GridAnswer::LeaveForward
        );
        assert_eq!(
            grid.key(press(Key::Tab, true, false), 10),
            GridAnswer::LeaveBack
        );
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
        assert_eq!(
            grid.key(press(Key::F2, false, false), 10),
            GridAnswer::OpenValue
        );
        assert_eq!(
            grid.key(press(Key::Character('x'), false, false), 10),
            GridAnswer::Ignored
        );
    }

    #[test]
    fn copy_makes_tsv_and_inserts_with_the_dialects_quoting() {
        let mut grid = customers();
        grid.anchor = Cell { row: 0, column: 0 };
        grid.current = Cell { row: 1, column: 1 };
        assert_eq!(grid.tsv(false), "1\tAda\n2\tGr\\tace\n");
        assert_eq!(grid.tsv(true), "id\tname\n1\tAda\n2\tGr\\tace\n");
        let GridAnswer::Copy { text, toast } =
            grid.key(press(Key::Character('c'), false, true), 10)
        else {
            panic!("Ctrl+C copies");
        };
        assert_eq!(
            (text.as_str(), toast.as_str()),
            ("1\tAda\n2\tGr\\tace\n", "Copied 4 cells")
        );
        grid.current = Cell { row: 1, column: 2 };
        assert_eq!(grid.tsv(false), "1\tAda\t\n2\tGr\\tace\tO'Hara\n");
        assert_eq!(
            grid.insert_statements(),
            "INSERT INTO customers (id, name, note) VALUES (1, 'Ada', NULL);\nINSERT INTO customers (id, name, note) VALUES (2, 'Gr\tace', 'O''Hara');\n"
        );
        let GridAnswer::Copy { toast, .. } = grid.key(press(Key::Character('I'), true, true), 10)
        else {
            panic!("Ctrl+Shift+I copies INSERTs");
        };
        assert_eq!(toast, "Copied 2 INSERT statements");
        grid.rows[0][1] = Some("A\nda".to_owned());
        let GridAnswer::Copy { toast, .. } = grid.key(press(Key::Character('I'), true, true), 10)
        else {
            panic!("Ctrl+Shift+I copies INSERTs");
        };
        assert_eq!(
            toast, "Copied 2 INSERT statements",
            "a line break is no statement"
        );
        grid.dialect = QueryDialect::MySql;
        grid.source_table = None;
        grid.anchor = Cell { row: 1, column: 2 };
        assert_eq!(
            grid.insert_statements(),
            "INSERT INTO result (note) VALUES ('O''Hara');\n"
        );
        assert_eq!(tsv_field("a\\b\r"), "a\\\\b\\r");
    }

    #[test]
    fn literals_and_names_follow_each_dialect() {
        assert_eq!(
            sql_literal(Some("it's"), ValueKind::Text, QueryDialect::PostgreSql),
            "'it''s'"
        );
        assert_eq!(
            sql_literal(Some("a\\b"), ValueKind::Text, QueryDialect::MySql),
            "'a\\\\b'"
        );
        assert_eq!(
            sql_literal(Some("é"), ValueKind::Text, QueryDialect::SqlServer),
            "N'é'"
        );
        assert_eq!(
            sql_literal(Some("-1.5e3"), ValueKind::Number, QueryDialect::PostgreSql),
            "-1.5e3"
        );
        assert_eq!(
            sql_literal(Some("NaN"), ValueKind::Number, QueryDialect::PostgreSql),
            "'NaN'"
        );
        assert_eq!(
            sql_literal(Some("t"), ValueKind::Boolean, QueryDialect::PostgreSql),
            "TRUE"
        );
        assert_eq!(
            sql_literal(Some("false"), ValueKind::Boolean, QueryDialect::SqlServer),
            "0"
        );
        assert_eq!(
            sql_literal(Some("\\x0aff"), ValueKind::Blob, QueryDialect::Sqlite),
            "X'0aff'"
        );
        assert_eq!(
            sql_literal(Some("\\x0aff"), ValueKind::Blob, QueryDialect::SqlServer),
            "0x0aff"
        );
        assert_eq!(
            sql_literal(Some("2024-01-02"), ValueKind::Text, QueryDialect::Oracle),
            "'2024-01-02'"
        );
        assert_eq!(
            sql_literal(None, ValueKind::Text, QueryDialect::Oracle),
            "NULL"
        );
        assert_eq!(quote_name("name", QueryDialect::PostgreSql), "name");
        assert_eq!(quote_name("Name", QueryDialect::PostgreSql), "\"Name\"");
        assert_eq!(quote_name("order", QueryDialect::MySql), "`order`");
        assert_eq!(quote_name("a b", QueryDialect::SqlServer), "[a b]");
        assert_eq!(quote_name("ID", QueryDialect::Oracle), "ID");
        assert_eq!(quote_name("say\"hi", QueryDialect::Sqlite), "\"say\"\"hi\"");
    }

    #[test]
    fn cells_draw_each_kind_as_the_spec_says() {
        assert_eq!(
            cell_text(None, ValueKind::Number),
            ("NULL".to_owned(), CellLook::Null)
        );
        assert_eq!(
            cell_text(Some("42"), ValueKind::Number),
            ("42".to_owned(), CellLook::Primary)
        );
        assert_eq!(cell_text(Some("a\tb"), ValueKind::Text).0, "a\u{2409}b");
        let twenty = format!("\\x{}", "ab".repeat(20));
        assert_eq!(
            cell_text(Some(&twenty), ValueKind::Blob),
            (format!("0x{}…", "ab".repeat(16)), CellLook::Secondary)
        );
        assert_eq!(cell_text(Some("\\x0102"), ValueKind::Blob).0, "0x0102");
        let vector = format!("[{}]", vec!["0.5"; 768].join(","));
        assert_eq!(
            cell_text(Some(&vector), ValueKind::Vector).0,
            "[0.5, 0.5, 0.5, … 768]"
        );
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

    #[test]
    fn the_wheel_scrolls_rows_and_whole_columns_and_keeps_the_current_cell() {
        let rows = (0..20)
            .map(|row| vec![Some(row.to_string()), None, None])
            .collect();
        let mut grid = ResultGrid::new(
            vec!["a".into(), "b".into(), "c".into()],
            vec![ValueKind::Number; 3],
            rows,
            20,
            vec![80.0, 120.0, 60.0],
        );
        grid.current = Cell { row: 2, column: 1 };
        grid.scroll_by(3, 0, 13);
        assert_eq!(grid.first_row, 3);
        grid.scroll_by(30, 0, 13);
        assert_eq!(grid.first_row, 7, "the last rows stay in view");
        grid.scroll_by(-30, 0, 13);
        assert_eq!(grid.first_row, 0);
        grid.scroll_by(0, 1, 13);
        assert_eq!(grid.scroll_x, 80.0);
        grid.scroll_by(0, 5, 13);
        assert_eq!(grid.scroll_x, 200.0);
        grid.scroll_by(0, -1, 13);
        assert_eq!(grid.scroll_x, 80.0);
        assert_eq!(grid.current, Cell { row: 2, column: 1 });
    }

    #[test]
    fn copy_as_csv_quotes_what_needs_it_and_leaves_null_empty() {
        let mut grid = customers();
        grid.rows[1][1] = Some("Grace, \"Amazing\"".into());
        grid.anchor = Cell { row: 0, column: 0 };
        grid.current = Cell { row: 1, column: 2 };
        assert_eq!(
            grid.csv(),
            "id,name,note\r\n1,Ada,\r\n2,\"Grace, \"\"Amazing\"\"\",O'Hara"
        );
        let GridAnswer::Copy { toast, .. } = grid.menu_item(GridMenuItem::CopyAsCsv) else {
            panic!("Copy as CSV copies");
        };
        assert_eq!(toast, "Copied 6 cells as CSV");
    }

    #[test]
    fn copy_as_csv_gives_json_cells_their_compact_text() {
        let grid = ResultGrid {
            anchor: Cell { row: 0, column: 0 },
            current: Cell { row: 1, column: 0 },
            ..ResultGrid::new(
                vec!["doc".into()],
                vec![ValueKind::Json],
                vec![
                    vec![Some(
                        "{\n  \"k\": [1, 2],\n  \"s\": \"a b\\\" c\"\n}".into(),
                    )],
                    vec![Some("{1,2}".into())],
                ],
                2,
                vec![60.0],
            )
        };
        assert_eq!(
            grid.csv(),
            "doc\r\n\"{\"\"k\"\":[1,2],\"\"s\"\":\"\"a b\\\"\" c\"\"}\"\r\n\"{1,2}\""
        );
    }

    #[test]
    fn copy_as_json_quotes_number_text_that_json_does_not_allow() {
        let rows = [".5", "-.5", "5.", "007", "-0.5", "1e3", "12"]
            .into_iter()
            .map(|text| vec![Some(text.to_owned())])
            .collect();
        let grid = ResultGrid {
            anchor: Cell { row: 0, column: 0 },
            current: Cell { row: 6, column: 0 },
            ..ResultGrid::new(
                vec!["n".into()],
                vec![ValueKind::Number],
                rows,
                7,
                vec![60.0],
            )
        };
        let json = grid.json();
        assert!(is_json(&json), "{json}");
        for quoted in ["\".5\"", "\"-.5\"", "\"5.\"", "\"007\""] {
            assert!(json.contains(&format!("\"n\": {quoted}")), "{json}");
        }
        for bare in ["-0.5", "1e3", "12"] {
            assert!(json.contains(&format!("\"n\": {bare}\n")), "{json}");
        }
    }

    #[test]
    fn copy_as_json_embeds_numbers_json_and_null_and_suffixes_repeated_names() {
        let grid = ResultGrid {
            anchor: Cell { row: 0, column: 0 },
            current: Cell { row: 1, column: 4 },
            ..ResultGrid::new(
                vec![
                    "id".into(),
                    "id".into(),
                    "ok".into(),
                    "doc".into(),
                    "data".into(),
                ],
                vec![
                    ValueKind::Number,
                    ValueKind::Text,
                    ValueKind::Boolean,
                    ValueKind::Json,
                    ValueKind::Blob,
                ],
                vec![
                    vec![
                        Some("1".into()),
                        Some("a\"b".into()),
                        Some("t".into()),
                        Some("{\"k\": [1, 2]}".into()),
                        Some("\\x0aff".into()),
                    ],
                    vec![
                        Some("NaN".into()),
                        None,
                        Some("f".into()),
                        Some("{1,2}".into()),
                        None,
                    ],
                ],
                2,
                vec![60.0; 5],
            )
        };
        assert_eq!(
            grid.json(),
            "[\n  {\n    \"id\": 1,\n    \"id_2\": \"a\\\"b\",\n    \"ok\": true,\n    \"doc\": {\n      \"k\": [\n        1,\n        2\n      ]\n    },\n    \"data\": \"0x0aff\"\n  },\n  {\n    \"id\": \"NaN\",\n    \"id_2\": null,\n    \"ok\": false,\n    \"doc\": \"{1,2}\",\n    \"data\": null\n  }\n]"
        );
        assert!(is_json("{\"a\": [1, -2.5e3, true, null, \"\\u00e9\"]}"));
        assert!(!is_json("{1,2}"));
        assert!(!is_json("[1,]"));
        assert!(!is_json(&"[".repeat(10_000)));
    }

    #[test]
    fn shift_f10_and_the_menu_key_open_the_menu() {
        let mut grid = customers();
        let shift_f10 = press(Key::F10, true, false);
        assert_eq!(grid.key(shift_f10, 10), GridAnswer::OpenMenu);
        assert_eq!(
            grid.key(press(Key::ContextMenu, false, false), 10),
            GridAnswer::OpenMenu
        );
        assert_eq!(
            grid.key(press(Key::F10, false, false), 10),
            GridAnswer::Ignored
        );
        assert_eq!(
            grid.menu_item(GridMenuItem::OpenValue),
            GridAnswer::OpenValue
        );
        grid.menu_item(GridMenuItem::SelectAll);
        assert_eq!(grid.selection(), (0..3, 0..3));
    }

    #[test]
    fn widths_stay_within_their_limits_and_survive_a_result_with_the_same_columns() {
        let mut grid = customers();
        assert!(grid.set_width(1, 2_000.0));
        assert_eq!(grid.widths[1], 960.0);
        grid.set_width(1, 10.0);
        assert_eq!(grid.widths[1], 48.0);
        assert!(!grid.set_width(7, 100.0));
        grid.rows[0][1] = Some("x".repeat(100));
        assert_eq!(grid.fit_width(1), 480.0);
        assert_eq!(grid.fit_width(0), 48.0);
        grid.set_width(2, 300.0);
        let mut again = customers();
        again.keep_widths_of(&grid);
        assert_eq!(again.widths, grid.widths);
        let mut other = ResultGrid::new(
            vec!["id".into(), "note".into(), "name".into()],
            vec![ValueKind::Number; 3],
            Vec::new(),
            0,
            vec![60.0; 3],
        );
        other.keep_widths_of(&grid);
        assert_eq!(other.widths, vec![60.0; 3]);
        assert_eq!(grid.width_text(2), "note · 300 px");
    }
}
