use super::modal::wrap;
use super::{Hint, Pane, READ_ONLY_TOAST, ToastKind, Ui};
use crate::frame::Frame;
use crate::source::Source;
use crate::terminal::{KeyCode, KeyEvent};
use crate::theme::{Role, Symbol};
use crate::width::{char_width, text_width};

pub(super) const DEFAULT_KEY_SPACE: &str = "default key space";
const LONGEST_NAME_BYTES: usize = 128;
const PICKER_COLS: usize = 46;
const SIDEBAR_FROM_COLS: u16 = 110;
const SIDEBAR_SMALLEST: u16 = 20;
const SIDEBAR_LARGEST: u16 = 24;
const PICKER_KEYS: &str = "Enter show   a add   Esc back";
const ADDING_KEYS: &str = "Enter add   Esc back";
const BAD_NAME: &str = "Collection names are 1 to 128 bytes with no spaces or control characters";

#[derive(Debug)]
pub(super) struct Picker {
    highlighted: usize,
    adding: Option<String>,
}

fn is_collection_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= LONGEST_NAME_BYTES
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub(super) fn cut_to_cells(text: &str, cells: usize, ellipsis: char) -> String {
    if text_width(text) <= cells {
        return text.to_owned();
    }
    let mut cut = String::new();
    let mut used = 0;
    for c in text.chars() {
        let width = char_width(c);
        if used + width + 1 > cells {
            break;
        }
        used += width;
        cut.push(c);
    }
    cut.push(ellipsis);
    cut
}

impl<S: Source> Ui<S> {
    pub(super) fn refresh_collections(&mut self) {
        match self.source.collections() {
            Ok(names) => self.collection_names = names,
            Err(error) => self.error(error),
        }
    }

    pub(super) fn has_collections(&self) -> bool {
        !self.collection_names.is_empty() || self.collection.is_some()
    }

    fn shown_row(&self) -> usize {
        match &self.collection {
            None => 0,
            Some(name) => self
                .collection_names
                .iter()
                .position(|known| known == name)
                .map_or(0, |index| index + 1),
        }
    }

    fn space_at(&self, row: usize) -> Option<Option<String>> {
        if row == 0 {
            return Some(None);
        }
        self.collection_names.get(row - 1).cloned().map(Some)
    }

    pub(super) fn open_picker(&mut self, adding: bool) {
        self.refresh_collections();
        let adding = (adding && !self.read_only()).then(String::new);
        self.picker = Some(Picker {
            highlighted: self.shown_row(),
            adding,
        });
    }

    pub(super) fn show_space(&mut self, space: Option<String>) {
        if let Err(error) = self.source.show_collection(space.as_deref()) {
            self.error(error);
            return;
        }
        let toast = match &space {
            Some(name) => format!("Showing collection {name}"),
            None => "Showing the default key space".to_owned(),
        };
        self.collection = space;
        self.filter.clear();
        self.filter_edit = None;
        self.viewer = None;
        self.restart_count();
        self.home();
        self.info(toast);
    }

    pub(super) fn picker_key(&mut self, key: KeyEvent) {
        let Some(mut picker) = self.picker.take() else {
            return;
        };
        if let Some(mut text) = picker.adding.take() {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if self.add_and_show_collection(&text) {
                        return;
                    }
                    picker.adding = Some(text);
                }
                KeyCode::Backspace => {
                    text.pop();
                    picker.adding = Some(text);
                }
                KeyCode::Char(c) if !key.modifiers.ctrl && !key.modifiers.alt => {
                    text.push(c);
                    picker.adding = Some(text);
                }
                _ => picker.adding = Some(text),
            }
            self.picker = Some(picker);
            return;
        }
        let last = self.collection_names.len();
        let page = self.page();
        match key.code {
            KeyCode::Esc | KeyCode::Char('C') => return,
            KeyCode::Char('q') => {
                self.quit = true;
                return;
            }
            KeyCode::Enter => {
                if let Some(space) = self.space_at(picker.highlighted)
                    && space != self.collection
                {
                    self.show_space(space);
                }
                return;
            }
            KeyCode::Char('a') if self.read_only() => self.info(READ_ONLY_TOAST),
            KeyCode::Char('a') => picker.adding = Some(String::new()),
            KeyCode::Down | KeyCode::Char('j') => {
                picker.highlighted = (picker.highlighted + 1).min(last);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                picker.highlighted = picker.highlighted.saturating_sub(1);
            }
            KeyCode::PageDown => picker.highlighted = (picker.highlighted + page).min(last),
            KeyCode::PageUp => picker.highlighted = picker.highlighted.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => picker.highlighted = 0,
            KeyCode::End | KeyCode::Char('G') => picker.highlighted = last,
            _ => {}
        }
        self.picker = Some(picker);
    }

    fn add_and_show_collection(&mut self, name: &str) -> bool {
        if !is_collection_name(name) {
            self.toast(ToastKind::Error, BAD_NAME.to_owned());
            return false;
        }
        if self.collection_names.iter().any(|known| known == name) {
            self.toast(
                ToastKind::Error,
                format!("Collection {name} already exists"),
            );
            return false;
        }
        match self.source.add_collection(name) {
            Ok(time) => {
                self.refresh_collections();
                self.show_space(Some(name.to_owned()));
                self.toast(
                    ToastKind::Success,
                    format!("Added collection {name} at {time}"),
                );
                true
            }
            Err(error) => {
                self.error(error);
                false
            }
        }
    }

    pub(super) fn sidebar_cols(&self) -> u16 {
        if self.size.cols < SIDEBAR_FROM_COLS || self.collection_names.is_empty() {
            return 0;
        }
        (self.size.cols / 6).clamp(SIDEBAR_SMALLEST, SIDEBAR_LARGEST)
    }

    pub(super) fn sidebar_offset(&self) -> u16 {
        match self.sidebar_cols() {
            0 => 0,
            cols => cols + 1,
        }
    }

    pub(super) fn next_pane(&self, forward: bool) -> Pane {
        let sidebar = self.sidebar_cols() > 0;
        match (self.focus, forward, sidebar) {
            (Pane::Keys, true, _) => Pane::Value,
            (Pane::Value, true, true) => Pane::Sidebar,
            (Pane::Value, true, false) | (Pane::Sidebar, true, _) => Pane::Keys,
            (Pane::Keys, false, true) => Pane::Sidebar,
            (Pane::Keys, false, false) | (Pane::Sidebar, false, _) => Pane::Value,
            (Pane::Value, false, _) => Pane::Keys,
        }
    }

    pub(super) fn sidebar_key(&mut self, code: KeyCode) -> bool {
        let shown = self.shown_row();
        let last = self.collection_names.len();
        let chosen = match code {
            KeyCode::Down | KeyCode::Char('j') => (shown + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => shown.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            KeyCode::Enter | KeyCode::Right | KeyCode::Esc => {
                self.focus = Pane::Keys;
                return true;
            }
            KeyCode::Char('a') if self.read_only() => {
                self.info(READ_ONLY_TOAST);
                return true;
            }
            KeyCode::Char('a') => {
                self.open_picker(true);
                return true;
            }
            _ => return false,
        };
        if chosen != shown
            && let Some(space) = self.space_at(chosen)
        {
            self.show_space(space);
        }
        true
    }

    pub(super) fn sidebar_hints(&self) -> Vec<Hint> {
        vec![
            Hint::new(self.theme.symbol(Symbol::UpDown), "switch"),
            Hint::new("a", "add").available(!self.read_only()),
            Hint::new("Enter", "keys"),
            Hint::new("C", "collection"),
            Hint::new("?", "help"),
            Hint::new("q", "quit"),
        ]
    }

    pub(super) fn keys_title(&self, width: u16, position_width: u16) -> String {
        let keys = self.pane_name(Pane::Keys, "Keys");
        let Some(name) = &self.collection else {
            return format!(" {keys}");
        };
        let filter_cells = if self.filter.is_empty() && self.filter_edit.is_none() {
            0
        } else {
            8 + 2
        };
        let fixed = text_width(&keys) + " in ".len() + 1;
        let room = usize::from(width.saturating_sub(position_width + 1))
            .saturating_sub(fixed + filter_cells)
            .max(1);
        format!(
            " {keys} in {}",
            cut_to_cells(name, room, self.theme.ellipsis())
        )
    }

    pub(super) fn empty_space_message(&self) -> String {
        let dash = self.theme.symbol(Symbol::EmDash);
        match &self.collection {
            Some(name) => format!("collection {name} is empty {dash} press a to add a key"),
            None => format!("empty database {dash} press a to add a key"),
        }
    }

    pub(super) fn draw_sidebar(&self, frame: &mut Frame, width: u16) {
        let role = self.title_role(Pane::Sidebar);
        frame.fill(0, 1, width, self.theme.style(role));
        let title = format!(" {}", self.pane_name(Pane::Sidebar, "Collections"));
        self.put(frame, 0, 1, &title, role, width);
        let visible = usize::from(self.size.rows.saturating_sub(4)).max(1);
        let shown = self.shown_row();
        let first = (shown + 1).saturating_sub(visible);
        let spaces = std::iter::once(DEFAULT_KEY_SPACE)
            .chain(self.collection_names.iter().map(String::as_str))
            .enumerate()
            .skip(first)
            .take(visible);
        for (offset, (index, label)) in spaces.enumerate() {
            let y = 2 + offset as u16;
            let selected = index == shown;
            let role = if selected {
                Role::Selected
            } else {
                Role::Plain
            };
            if selected {
                frame.fill(0, y, width, self.theme.style(role));
            }
            let marker = if selected { " > " } else { "   " };
            let used = self.put(frame, 0, y, marker, role, width);
            self.put(frame, used, y, label, role, width.saturating_sub(used));
        }
    }

    pub(super) fn draw_picker(&self, frame: &mut Frame, picker: &Picker) {
        let cols = usize::from(self.size.cols);
        let rows = usize::from(self.size.rows);
        let width = PICKER_COLS.min(cols.saturating_sub(4)).max(12);
        let inner = width - 4;
        let file = self
            .summary
            .as_ref()
            .map(|summary| summary.name.clone())
            .unwrap_or_default();
        let mut footer: Vec<String> = Vec::new();
        if self.collection_names.is_empty() {
            let text = if self.read_only() {
                format!(
                    "{file} has no collections. Quit, then add one with eggdb collection add {file} <name>."
                )
            } else {
                format!("{file} has no collections yet. Press a to add one.")
            };
            footer.extend(wrap(&text, inner));
        } else {
            footer.extend(wrap("Delete one after quitting, with", inner));
            footer.extend(wrap(
                &format!("eggdb collection delete {file} <name>"),
                inner,
            ));
        }
        let most_list = rows.saturating_sub(8 + footer.len()).max(1);
        let total = self.collection_names.len() + 1;
        let list_rows = total.min(most_list);
        let first = (picker.highlighted + 1).saturating_sub(list_rows);
        let height = list_rows + footer.len() + 5;
        let col = u16::try_from(cols.saturating_sub(width) / 2).unwrap_or(0);
        let top = u16::try_from(rows.saturating_sub(height) / 2).unwrap_or(0);
        let width_cells = u16::try_from(width).unwrap_or(self.size.cols);
        let plain = self.theme.style(Role::Plain);
        let horizontal = self.theme.symbol(Symbol::HorizontalLine);
        let vertical = self.theme.symbol(Symbol::VerticalLine);
        let title = match &picker.adding {
            Some(text) => format!("Add collection: {text}"),
            None => "Collections".to_owned(),
        };
        let title_shown = cut_to_cells(&title, width.saturating_sub(4), self.theme.ellipsis());
        let top_line = format!(
            "{} {title_shown} {}{}",
            self.theme.symbol(Symbol::TopLeft),
            horizontal.repeat(width.saturating_sub(text_width(&title_shown) + 4)),
            self.theme.symbol(Symbol::TopRight)
        );
        frame.fill(col, top, width_cells, plain);
        self.put(frame, col, top, &top_line, Role::Plain, width_cells);
        if picker.adding.is_some() {
            let at = col + 2 + u16::try_from(text_width(&title_shown)).unwrap_or(0);
            frame.cursor = Some((at.min(col + width_cells - 2), top));
        }
        let text_room = u16::try_from(inner).unwrap_or(width_cells);
        let mut row = top;
        let mut body = |frame: &mut Frame, text: &str, role: Role, centred: bool| {
            row += 1;
            frame.fill(col, row, width_cells, plain);
            self.put(frame, col, row, vertical, Role::Plain, 1);
            let used = u16::try_from(text_width(text)).unwrap_or(text_room);
            let start = if centred {
                col + 2 + text_room.saturating_sub(used) / 2
            } else {
                col + 2
            };
            self.put(frame, start, row, text, role, text_room);
            self.put(frame, col + width_cells - 1, row, vertical, Role::Plain, 1);
        };
        let spaces = std::iter::once(DEFAULT_KEY_SPACE)
            .chain(self.collection_names.iter().map(String::as_str))
            .enumerate()
            .skip(first)
            .take(list_rows);
        for (index, label) in spaces {
            let highlighted = index == picker.highlighted;
            let marker = if highlighted { "> " } else { "  " };
            let role = if highlighted {
                Role::Selected
            } else {
                Role::Plain
            };
            body(frame, &format!("{marker}{label}"), role, false);
        }
        body(frame, "", Role::Plain, false);
        for line in &footer {
            body(frame, line, Role::Plain, false);
        }
        body(frame, "", Role::Plain, false);
        let keys = if picker.adding.is_some() {
            ADDING_KEYS
        } else {
            PICKER_KEYS
        };
        body(frame, keys, Role::Plain, true);
        let bottom_row = row + 1;
        let bottom = format!(
            "{}{}{}",
            self.theme.symbol(Symbol::BottomLeft),
            horizontal.repeat(width.saturating_sub(2)),
            self.theme.symbol(Symbol::BottomRight)
        );
        frame.fill(col, bottom_row, width_cells, plain);
        self.put(frame, col, bottom_row, &bottom, Role::Plain, width_cells);
    }
}

#[cfg(test)]
mod tests {
    use super::{cut_to_cells, is_collection_name};

    #[test]
    fn names_are_cut_to_the_cells_they_have() {
        assert_eq!(cut_to_cells("users", 10, '…'), "users");
        assert_eq!(cut_to_cells("a-very-long-collection", 8, '…'), "a-very-…");
        assert_eq!(cut_to_cells("日本語の名前", 5, '…'), "日本…");
    }

    #[test]
    fn collection_names_follow_the_engine_rule() {
        assert!(is_collection_name("users"));
        assert!(!is_collection_name(""));
        assert!(!is_collection_name("two words"));
        assert!(!is_collection_name(&"n".repeat(129)));
    }
}
