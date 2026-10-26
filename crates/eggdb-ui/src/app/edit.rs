use super::modal::{Modal, Pending};
use super::{Hint, Pane, ToastKind, Ui, help_editing_line};
use crate::editor::{Editor, LONGEST_TEXT, Lines, Outcome};
use crate::frame::Frame;
use crate::source::{Change, Source, Start, WriterBusy};
use crate::terminal::KeyEvent;
use crate::text::{self, LineBreaks};
use crate::theme::{Role, Symbol};
use crate::width::text_width;

const MULTI_LINE_FROM_BYTES: usize = 1024;
const GUTTER_FROM_COLS: u16 = 60;
const KEY_ENTRY_TITLE: &str = " Add key: ";

#[derive(Debug)]
pub(super) enum Edit {
    NewKey { editor: Editor, return_focus: Pane },
    Value(ValueEdit),
}

#[derive(Debug)]
pub(super) struct ValueEdit {
    key: Vec<u8>,
    editor: Editor,
    stored_bytes: Option<u64>,
    adding: bool,
    mode: &'static str,
    decoded_bytes: Option<u64>,
    return_focus: Pane,
}

impl Edit {
    pub(super) fn return_focus(&self) -> Pane {
        match self {
            Self::NewKey { return_focus, .. } => *return_focus,
            Self::Value(edit) => edit.return_focus,
        }
    }
}

pub(super) fn is_text(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|text| {
        !text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
    })
}

fn decoded_bytes(text: &str) -> Option<u64> {
    text::decoded(text).ok().map(|bytes| bytes.len() as u64)
}

fn bad_escape_message(editor: &Editor, offset: usize, expected: &str) -> String {
    let before = &editor.text()[..offset];
    let line = before.matches('\n').count();
    let column = before[before.rfind('\n').map_or(0, |at| at + 1)..]
        .chars()
        .count()
        + 1;
    if editor.lines() == Lines::Multi {
        format!(
            "Bad escape at line {}, column {column}: expected {expected}",
            line + 1
        )
    } else {
        format!("Bad escape at column {column}: expected {expected}")
    }
}

impl<S: Source> Ui<S> {
    pub(super) fn has_unsaved_edit(&self) -> bool {
        matches!(&self.edit, Some(Edit::Value(edit)) if edit.editor.is_changed())
    }

    pub(super) fn adding(&self) -> bool {
        matches!(&self.edit, Some(Edit::Value(edit)) if edit.adding)
    }

    fn too_large_to_edit(&mut self, bytes: u64) {
        let name = self
            .summary
            .as_ref()
            .map(|summary| summary.name.clone())
            .unwrap_or_default();
        self.toast(
            ToastKind::Error,
            format!(
                "Value is {}; the editor holds up to {} of text. Store larger values with eggdb put {name} <key> @file",
                text::short_size(bytes),
                text::file_size(LONGEST_TEXT as u64)
            ),
        );
    }

    pub(super) fn start_edit(&mut self) {
        let Some(viewer) = &self.viewer else {
            self.info("No key selected; a adds one");
            return;
        };
        if viewer.missing {
            self.info("This key no longer exists; press r to re-read");
            return;
        }
        let value_bytes = viewer.value_bytes;
        let bytes = match viewer.bytes.as_deref() {
            Some(bytes) if bytes.len() <= LONGEST_TEXT => bytes,
            _ => {
                self.too_large_to_edit(value_bytes);
                return;
            }
        };
        let text_with_lines = is_text(bytes) && bytes.contains(&b'\n');
        let lines = if text_with_lines || bytes.len() >= MULTI_LINE_FROM_BYTES {
            Lines::Multi
        } else {
            Lines::Single
        };
        let spelled = if text_with_lines {
            text::spelled(bytes, LineBreaks::Kept)
        } else {
            text::escaped(bytes)
        };
        if spelled.len() > LONGEST_TEXT {
            self.too_large_to_edit(value_bytes);
            return;
        }
        let mut editor = Editor::new(spelled, lines);
        editor.move_to(0);
        self.edit = Some(Edit::Value(ValueEdit {
            key: viewer.key.clone(),
            editor,
            stored_bytes: Some(value_bytes),
            adding: false,
            mode: viewer.mode.name(),
            decoded_bytes: Some(value_bytes),
            return_focus: self.focus,
        }));
        self.focus = Pane::Value;
    }

    pub(super) fn start_add(&mut self) {
        self.edit = Some(Edit::NewKey {
            editor: Editor::new(String::new(), Lines::Single),
            return_focus: self.focus,
        });
        self.focus = Pane::Keys;
    }

    pub(super) fn start_delete(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            self.info("No key selected");
            return;
        };
        let pending = Pending {
            change: Change::Delete {
                key: row.key.clone(),
            },
            replaced_bytes: Some(row.value_bytes),
        };
        self.confirm_delete(pending);
    }

    pub(super) fn edit_paste(&mut self, pasted: &str) {
        let outcome = match self.edit.as_mut() {
            Some(Edit::NewKey { editor, .. }) => editor.paste_on_one_line(pasted),
            Some(Edit::Value(edit)) => edit.editor.paste(pasted),
            None => return,
        };
        self.after_edit(outcome);
    }

    pub(super) fn edit_key(&mut self, key: KeyEvent) {
        let page = self.viewer_rows();
        let outcome = match self.edit.as_mut() {
            Some(Edit::NewKey { .. }) if key.modifiers.alt => Outcome::Ignored,
            Some(Edit::NewKey { editor, .. }) => editor.key(key, 1),
            Some(Edit::Value(edit)) => edit.editor.key(key, page),
            None => return,
        };
        self.after_edit(outcome);
    }

    fn after_edit(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Changed => {
                if let Some(Edit::Value(edit)) = self.edit.as_mut() {
                    edit.decoded_bytes = decoded_bytes(edit.editor.text());
                }
            }
            Outcome::Confirm => self.confirm_edit(),
            Outcome::Cancel if self.has_unsaved_edit() => {
                self.modal = Some(Modal::discard_question());
            }
            Outcome::Cancel => self.close_edit(),
            Outcome::Help => self.help = Some(help_editing_line(self)),
            Outcome::TooLong => self.toast(
                ToastKind::Error,
                format!(
                    "The editor holds up to {} of text; store larger values with eggdb put",
                    text::file_size(LONGEST_TEXT as u64)
                ),
            ),
            Outcome::Moved | Outcome::Ignored => {}
        }
    }

    fn confirm_edit(&mut self) {
        let max_key = self
            .summary
            .as_ref()
            .map_or(usize::MAX, |summary| summary.max_key_bytes);
        match self.edit.take() {
            Some(Edit::NewKey {
                mut editor,
                return_focus,
            }) => match text::decoded(editor.text()) {
                Err(bad) => {
                    let message = bad_escape_message(&editor, bad.offset, bad.expected);
                    editor.move_to(bad.offset);
                    self.edit = Some(Edit::NewKey {
                        editor,
                        return_focus,
                    });
                    self.toast(ToastKind::Error, message);
                }
                Ok(key) if key.len() > max_key => {
                    self.edit = Some(Edit::NewKey {
                        editor,
                        return_focus,
                    });
                    self.toast(
                        ToastKind::Error,
                        format!(
                            "Key is {}; this database's keys are at most {}",
                            text::value_size(key.len() as u64),
                            text::value_size(max_key as u64)
                        ),
                    );
                }
                Ok(key) => {
                    self.edit = Some(Edit::Value(ValueEdit {
                        key,
                        editor: Editor::new(String::new(), Lines::Single),
                        stored_bytes: None,
                        adding: true,
                        mode: "text",
                        decoded_bytes: Some(0),
                        return_focus,
                    }));
                    self.focus = Pane::Value;
                }
            },
            Some(Edit::Value(mut edit)) => match text::decoded(edit.editor.text()) {
                Err(bad) => {
                    let message = bad_escape_message(&edit.editor, bad.offset, bad.expected);
                    edit.editor.move_to(bad.offset);
                    self.edit = Some(Edit::Value(edit));
                    self.toast(ToastKind::Error, message);
                }
                Ok(value) => {
                    let replaced_bytes = if edit.adding {
                        self.stored_size(&edit.key)
                    } else {
                        edit.stored_bytes
                    };
                    let pending = Pending {
                        change: Change::Put {
                            key: edit.key.clone(),
                            value,
                        },
                        replaced_bytes,
                    };
                    self.edit = Some(Edit::Value(edit));
                    self.confirm_put(pending);
                }
            },
            None => {}
        }
    }

    fn stored_size(&mut self, key: &[u8]) -> Option<u64> {
        match self.source.keys(key, Start::AtOrAfter(key), 1) {
            Ok(rows) => rows
                .into_iter()
                .find(|row| row.key == key)
                .map(|row| row.value_bytes),
            Err(error) => {
                self.error(error);
                None
            }
        }
    }

    pub(super) fn commit(&mut self, pending: Pending, busy: WriterBusy) {
        match self.source.write(&pending.change, busy) {
            Ok(time) => self.committed(pending, &time),
            Err(error) => self.commit_failed(pending, error),
        }
    }

    fn committed(&mut self, pending: Pending, time: &str) {
        let key = pending.change.key().to_vec();
        let shown = key.starts_with(&self.filter);
        let (delta, verb, bytes) = match &pending.change {
            Change::Put { value, .. } => (
                i64::from(pending.replaced_bytes.is_none()),
                "Committed",
                value.len() as u64,
            ),
            Change::Delete { .. } => (-1, "Deleted", pending.replaced_bytes.unwrap_or(0)),
        };
        if shown && delta != 0 {
            if self.counting {
                self.restart_count();
            } else {
                self.count = self.count.saturating_add_signed(delta);
            }
        }
        self.modal = None;
        self.close_edit();
        self.show_after_commit(&key, delta < 0);
        let mut message = format!(
            "{verb} {} ({}) at {time}",
            text::escaped(&key),
            text::value_size(bytes)
        );
        if !shown {
            message.push_str(&format!(
                "; not shown by filter /{}",
                text::escaped(&self.filter)
            ));
        }
        self.toast(ToastKind::Success, message);
    }

    fn show_after_commit(&mut self, key: &[u8], deleted: bool) {
        self.summary = self.source.summary();
        let anchor = self.rows.first().map(|row| row.key.clone());
        let page = self.page();
        self.rows = match &anchor {
            Some(anchor) => self.fetch(Start::AtOrAfter(anchor), page),
            None => self.fetch(Start::First, page),
        };
        let last = self.rows.len().saturating_sub(1);
        if deleted {
            self.selected = self
                .rows
                .iter()
                .position(|row| row.key.as_slice() > key)
                .unwrap_or(last);
        } else if let Some(at) = self.rows.iter().position(|row| row.key == key) {
            self.selected = at;
        } else if key.starts_with(&self.filter) {
            self.rows = self.fetch(Start::AtOrAfter(key), page);
            self.selected = 0;
            self.first_index = None;
        } else {
            self.selected = self.selected.min(last);
        }
        self.refit();
        self.viewer = None;
        self.selection_changed();
    }

    pub(super) fn edit_hints(&self) -> Option<Vec<Hint>> {
        Some(match self.edit.as_ref()? {
            Edit::NewKey { .. } => vec![
                Hint::new("Enter", "continue"),
                Hint::new("Esc", "cancel"),
                Hint::new("\\xNN", "a byte"),
                Hint::new("F1", "help"),
            ],
            Edit::Value(edit) if edit.editor.lines() == Lines::Single => vec![
                Hint::new("Enter", "confirm"),
                Hint::new("Alt-Enter", "new line"),
                Hint::new("Esc", "cancel"),
                Hint::new("\\xNN", "a byte"),
                Hint::new("F1", "help"),
            ],
            Edit::Value(_) => vec![
                Hint::new("Ctrl-S", "confirm"),
                Hint::new("Ctrl-Z", "undo"),
                Hint::new("Ctrl-Y", "redo"),
                Hint::new("Esc", "cancel"),
                Hint::new("F1", "help"),
            ],
        })
    }

    fn editor_area(&self, edit: &ValueEdit) -> (u16, u16, usize) {
        let pane = if self.two_panes() {
            self.viewer_cols()
        } else {
            self.size.cols
        };
        let gutter = if edit.editor.lines() == Lines::Multi && pane >= GUTTER_FROM_COLS {
            u16::try_from(edit.editor.line_count().to_string().len().max(2) + 1).unwrap_or(0)
        } else {
            0
        };
        let rows = match edit.editor.lines() {
            Lines::Single => 1,
            Lines::Multi => self.viewer_rows().max(1),
        };
        (pane, gutter, rows)
    }

    pub(super) fn follow_editor_cursor(&mut self) {
        let key_entry_cols = if self.two_panes() {
            self.list_cols()
        } else {
            self.size.cols
        };
        let value_area = match &self.edit {
            Some(Edit::Value(edit)) => Some(self.editor_area(edit)),
            _ => None,
        };
        match self.edit.as_mut() {
            Some(Edit::NewKey { editor, .. }) => {
                let room = usize::from(key_entry_cols).saturating_sub(KEY_ENTRY_TITLE.len() + 1);
                editor.scroll_into_view(1, room);
            }
            Some(Edit::Value(edit)) => {
                if let Some((pane, gutter, rows)) = value_area {
                    let room = usize::from(pane.saturating_sub(2 + gutter));
                    edit.editor.scroll_into_view(rows, room);
                }
            }
            None => {}
        }
    }

    pub(super) fn draw_key_entry(&self, frame: &mut Frame, col: u16, width: u16) -> bool {
        let Some(Edit::NewKey { editor, .. }) = &self.edit else {
            return false;
        };
        frame.fill(col, 1, width, self.theme.style(Role::FocusedTitle));
        let used = self.put(frame, col, 1, KEY_ENTRY_TITLE, Role::FocusedTitle, width);
        let start = col + used;
        let room = width.saturating_sub(used);
        let segments = [(editor.text().to_owned(), Role::FocusedTitle)];
        self.draw_segments(frame, start, 1, room, &segments, editor.pan());
        let cursor = editor.cursor_cells().saturating_sub(editor.pan());
        let at = start.saturating_add(u16::try_from(cursor).unwrap_or(u16::MAX));
        frame.cursor = Some((at.min(col + width.saturating_sub(1)), 1));
        true
    }

    pub(super) fn draw_value_editor(&self, frame: &mut Frame, col: u16, width: u16) -> bool {
        let Some(Edit::Value(edit)) = &self.edit else {
            return false;
        };
        let (_, gutter, rows) = self.editor_area(edit);
        let arrow = self.theme.symbol(Symbol::RightArrow);
        let dot = self.theme.symbol(Symbol::MiddleDot);
        let before = edit
            .stored_bytes
            .map_or_else(|| "new".to_owned(), text::value_size);
        let after = edit
            .decoded_bytes
            .map_or_else(|| "?".to_owned(), text::value_size);
        let verb = if edit.adding { "Add" } else { "Edit" };
        let title = format!(
            " {verb}  {} {dot} {before} {arrow} {after}  {}",
            edit.mode,
            text::escaped(&edit.key)
        );
        let position = format!(
            "ln {} col {} ",
            text::count(edit.editor.cursor_line() as u64 + 1),
            text::count(edit.editor.cursor_col() as u64 + 1)
        );
        frame.fill(col, 1, width, self.theme.style(Role::FocusedTitle));
        let position_width = u16::try_from(text_width(&position)).unwrap_or(width);
        self.put(
            frame,
            col,
            1,
            &title,
            Role::FocusedTitle,
            width.saturating_sub(position_width + 1),
        );
        self.put_right(frame, col + width, 1, &position, Role::FocusedTitle);
        self.put(
            frame,
            col + 1,
            2,
            &text::escaped(&edit.key),
            Role::Plain,
            width.saturating_sub(1),
        );
        let rule = self
            .theme
            .symbol(Symbol::HorizontalLine)
            .repeat(usize::from(width));
        self.put(frame, col, 3, &rule, Role::Plain, width);
        let text_col = col + 1 + gutter;
        let room = width.saturating_sub(1 + gutter);
        for (index, (line_number, line)) in edit.editor.visible_lines(rows).enumerate() {
            let row = 4 + index as u16;
            if gutter > 0 {
                let number = format!(
                    "{:>width$} ",
                    line_number + 1,
                    width = usize::from(gutter - 1)
                );
                self.put(frame, col + 1, row, &number, Role::Dim, gutter);
            }
            let segments = [(line.to_owned(), Role::Plain)];
            self.draw_segments(frame, text_col, row, room, &segments, edit.editor.pan());
        }
        let cursor_row = edit.editor.cursor_line().saturating_sub(edit.editor.top());
        let cursor_col = edit.editor.cursor_cells().saturating_sub(edit.editor.pan());
        frame.cursor = Some((
            text_col
                .saturating_add(u16::try_from(cursor_col).unwrap_or(u16::MAX))
                .min(col + width.saturating_sub(1)),
            4 + u16::try_from(cursor_row).unwrap_or(0),
        ));
        true
    }
}
