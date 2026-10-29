use super::edit::is_text;
use super::{Hint, Mode, ToastKind, Ui};
use crate::frame::Frame;
use crate::source::{Change, ErrorKind, Source, SourceError, WriterBusy};
use crate::terminal::{KeyCode, KeyEvent};
use crate::text;
use crate::theme::{Role, Symbol};
use crate::time_to_live::format_time_left;
use crate::width::{char_width, text_width};
use std::time::Duration;

const SMALLEST_TEXT_WIDTH: usize = 30;
const PREVIEW_BYTES: usize = 256;
const KEY_GAP: &str = "     ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Pending {
    pub(super) change: Change,
    pub(super) replaced_bytes: Option<u64>,
}

#[derive(Debug)]
pub(super) enum ModalKind {
    ConfirmPut(Pending),
    ConfirmDelete(Pending),
    ConfirmExpiry(Pending),
    WriterBusy { pending: Pending, waiting: bool },
    Reopen,
    Corrupt,
    Failed,
    Message,
    DiscardEdit,
    QuitWithEdit,
}

#[derive(Debug)]
pub(super) struct Modal {
    pub(super) kind: ModalKind,
    title: String,
    body: Vec<BodyLine>,
    keys: Vec<(&'static str, &'static str)>,
    scroll: usize,
    shown: bool,
    armed: bool,
}

#[derive(Debug)]
struct BodyLine {
    text: String,
    role: Role,
    wrap: Wrap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wrap {
    AtSpaces,
    AnyCharacter,
}

struct Layout {
    col: u16,
    row: u16,
    width: u16,
    text_width: usize,
    lines: Vec<(String, Role)>,
    visible: usize,
}

impl Modal {
    fn new(kind: ModalKind, title: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            body: Vec::new(),
            keys: Vec::new(),
            scroll: 0,
            shown: false,
            armed: false,
        }
    }

    fn line(self, text: impl Into<String>) -> Self {
        self.styled(text, Role::Plain)
    }

    fn styled(mut self, text: impl Into<String>, role: Role) -> Self {
        self.body.push(BodyLine {
            text: text.into(),
            role,
            wrap: Wrap::AtSpaces,
        });
        self
    }

    fn key_line(mut self, key: &[u8]) -> Self {
        self.body.push(BodyLine {
            text: format!(
                "Key    {} ({})",
                text::escaped(key),
                text::value_size(key.len() as u64)
            ),
            role: Role::Plain,
            wrap: Wrap::AnyCharacter,
        });
        self
    }

    fn in_collection(self, collection: Option<&str>) -> Self {
        match collection {
            Some(name) => self.line(format!("In     collection {name}")),
            None => self,
        }
    }

    fn key(mut self, key: &'static str, label: &'static str) -> Self {
        self.keys.push((key, label));
        self
    }

    pub(super) fn quit_question(stay: &'static str) -> Self {
        Self::new(ModalKind::QuitWithEdit, "Quit")
            .line("Your edit is not committed; quitting now discards it.")
            .key("Ctrl-C", "quit")
            .key("Esc", stay)
    }

    pub(super) fn discard_question() -> Self {
        Self::new(ModalKind::DiscardEdit, "Discard edit")
            .line("Your edit is not committed; nothing has changed in the database.")
            .key("y", "discard it")
            .key("Esc", "keep editing")
    }

    pub(super) fn presented(&mut self, visible: bool) {
        self.shown = visible;
        self.armed &= visible;
    }

    pub(super) fn awaits_quiet(&self) -> bool {
        self.shown && !self.armed
    }

    pub(super) fn arm_if_shown(&mut self) {
        self.armed = self.shown;
    }

    pub(super) fn asks_to_quit(&self) -> bool {
        matches!(self.kind, ModalKind::QuitWithEdit)
    }

    pub(super) fn is_waiting(&self) -> bool {
        matches!(self.kind, ModalKind::WriterBusy { waiting: true, .. })
    }

    pub(super) fn hints(&self) -> Vec<Hint> {
        self.keys
            .iter()
            .map(|(key, label)| Hint::new(key, label))
            .collect()
    }
}

pub(super) fn error_line(error: &SourceError) -> String {
    format!("Error ({}): {}", error.kind.name(), error.message)
}

fn wrap_characters(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = vec![String::new()];
    let mut used = 0;
    for c in text.chars() {
        let c_width = char_width(c);
        if used + c_width > width && used > 0 {
            lines.push(String::new());
            used = 0;
        }
        if let Some(line) = lines.last_mut() {
            line.push(c);
        }
        used += c_width;
    }
    lines
}

pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for word in paragraph.split(' ') {
            let word_width = text_width(word);
            if used > 0 && used + 1 + word_width <= width {
                line.push(' ');
                line.push_str(word);
                used += 1 + word_width;
                continue;
            }
            if used > 0 {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            for c in word.chars() {
                let c_width = char_width(c);
                if used + c_width > width && used > 0 {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(c);
                used += c_width;
            }
        }
        lines.push(line);
    }
    lines
}

fn value_kind(bytes: &[u8]) -> &'static str {
    if is_text(bytes) { "text" } else { "binary" }
}

impl<S: Source> Ui<S> {
    pub(super) fn confirm_put(&mut self, pending: Pending) {
        let Change::Put { key, value } = &pending.change else {
            return;
        };
        let key = key.clone();
        let new_size = text::value_size(value.len() as u64);
        let kind = value_kind(value);
        let (title, value_line) = match (pending.replaced_bytes, self.adding()) {
            (None, _) => ("Add key", format!("Value  {new_size}, {kind}")),
            (Some(old), true) => (
                "Replace value",
                format!(
                    "Value  already exists: {} {} {new_size}, {kind}",
                    text::value_size(old),
                    self.theme.symbol(Symbol::RightArrow)
                ),
            ),
            (Some(old), false) => (
                "Replace value",
                format!(
                    "Value  {} {} {new_size}, {kind}",
                    text::value_size(old),
                    self.theme.symbol(Symbol::RightArrow)
                ),
            ),
        };
        self.modal = Some(
            Modal::new(ModalKind::ConfirmPut(pending), title)
                .key_line(&key)
                .in_collection(self.collection.as_deref())
                .line(value_line)
                .line("")
                .line("One commit, written to disk before you continue.")
                .key("Enter", "commit")
                .key("Esc", "back to editor"),
        );
    }

    pub(super) fn confirm_delete(&mut self, pending: Pending) {
        let key = pending.change.key().to_vec();
        let size = text::value_size(pending.replaced_bytes.unwrap_or(0));
        let value_line = match self.viewer.as_ref().filter(|viewer| viewer.key == key) {
            Some(viewer) => match viewer.bytes.as_deref() {
                Some(_) if viewer.mode == Mode::Hex => format!("Value  {size}, binary"),
                Some(bytes) => {
                    let shown = &bytes[..bytes.len().min(PREVIEW_BYTES)];
                    let first = shown.split(|&b| b == b'\n').next().unwrap_or_default();
                    format!("Value  {size}, text: {}", text::escaped(first))
                }
                None => format!("Value  {size}"),
            },
            None => format!("Value  {size}"),
        };
        self.modal = Some(
            Modal::new(ModalKind::ConfirmDelete(pending), "Delete key")
                .key_line(&key)
                .in_collection(self.collection.as_deref())
                .line(value_line)
                .line("")
                .line("Deletes this one key in one commit, written to disk before you continue. There is no undo.")
                .key("d", "delete")
                .key("Esc", "keep"),
        );
    }

    pub(super) fn confirm_expiry(&mut self, pending: Pending, left: Option<Duration>) {
        let key = pending.change.key().to_vec();
        let arrow = self.theme.symbol(Symbol::RightArrow);
        let shown = |time: Option<Duration>| time.map_or_else(|| "never".to_owned(), format_time_left);
        let (title, after) = match &pending.change {
            Change::Expire { time_to_live, .. } => ("Set time to live", Some(*time_to_live)),
            _ => ("Clear time to live", None),
        };
        self.modal = Some(
            Modal::new(ModalKind::ConfirmExpiry(pending), title)
                .key_line(&key)
                .in_collection(self.collection.as_deref())
                .line(format!("TTL    {} {arrow} {}", shown(left), shown(after)))
                .line("")
                .line("One commit, written to disk before you continue.")
                .key("Enter", "commit")
                .key("Esc", "back"),
        );
    }

    pub(super) fn commit_failed(&mut self, pending: Pending, error: SourceError) {
        let line = error_line(&error);
        self.modal = match error.kind {
            ErrorKind::WriterActive => Some(
                Modal::new(
                    ModalKind::WriterBusy {
                        pending,
                        waiting: false,
                    },
                    "Writer busy",
                )
                .line("Another write transaction is open on this database.")
                .line("")
                .styled(line, Role::Error)
                .line("")
                .line(format!(
                    "Try: r waits up to {} for it to end, then commits.",
                    self.write_timeout_text()
                ))
                .key("r", "retry")
                .key("Esc", "back"),
            ),
            ErrorKind::TooLarge | ErrorKind::CacheFull => {
                self.toast(
                    ToastKind::Error,
                    format!(
                        "{line}\nTry: start eggdb ui with --cache 64M, or store a smaller value."
                    ),
                );
                None
            }
            ErrorKind::ReadOnly => {
                self.error(error);
                None
            }
            ErrorKind::NotAnInteger => {
                self.toast(
                    ToastKind::Error,
                    format!("{line}\nTry: e to store a whole number"),
                );
                None
            }
            ErrorKind::NeedsReopen => Some(
                Modal::new(ModalKind::Reopen, "Reopen needed")
                    .line("A write failed and the database must be reopened to recover its last durable state. Your edit is kept and offered again after the reopen.")
                    .line("")
                    .styled(line, Role::Error)
                    .key("r", "reopen")
                    .key("q", "quit"),
            ),
            ErrorKind::Corrupt => Some(
                Modal::new(ModalKind::Corrupt, "Damaged database")
                    .styled(line, Role::Error)
                    .line("")
                    .line("Nothing was committed.")
                    .line("Try: c checks every page; restore the file from a backup if the check fails.")
                    .key("c", "check")
                    .key("Esc", "back"),
            ),
            _ => Some(
                Modal::new(ModalKind::Failed, "Commit failed")
                    .styled(line, Role::Error)
                    .line("")
                    .line("Nothing was committed.")
                    .key("Esc", "back"),
            ),
        };
    }

    fn write_timeout_text(&self) -> String {
        let seconds = self
            .summary
            .as_ref()
            .map_or(0.0, |summary| summary.write_timeout.as_secs_f64());
        format!("{seconds} s")
    }

    pub(super) fn write_waiting(&self) -> bool {
        self.modal.as_ref().is_some_and(Modal::is_waiting)
    }

    pub(super) fn retry_write(&mut self) {
        if let Some(Modal {
            kind: ModalKind::WriterBusy { pending, .. },
            ..
        }) = self.modal.take()
        {
            self.commit(pending, WriterBusy::Wait);
        }
    }

    pub(super) fn show_last_message(&mut self) {
        let Some(last) = self.last_message.clone() else {
            self.info("No message yet");
            return;
        };
        let role = last.kind.role();
        let title = match last.kind {
            ToastKind::Error => "Error",
            ToastKind::Info | ToastKind::Success => "Message",
        };
        let mut modal = Modal::new(ModalKind::Message, title);
        for line in last.text.split('\n') {
            modal = modal.styled(line, role);
        }
        self.modal = Some(modal.key("Esc", "close"));
    }

    pub(super) fn modal_key(&mut self, key: KeyEvent) {
        let Some(mut modal) = self.modal.take() else {
            return;
        };
        if !modal.armed || modal.is_waiting() || self.scroll_modal(&mut modal, key.code) {
            self.modal = Some(modal);
            return;
        }
        let kind = std::mem::replace(&mut modal.kind, ModalKind::Failed);
        match (kind, key.code) {
            (ModalKind::ConfirmPut(pending), KeyCode::Enter)
            | (ModalKind::ConfirmExpiry(pending), KeyCode::Enter)
            | (ModalKind::ConfirmDelete(pending), KeyCode::Char('d')) => {
                self.commit(pending, WriterBusy::Fail);
            }
            (ModalKind::WriterBusy { pending, .. }, KeyCode::Char('r')) => {
                modal.kind = ModalKind::WriterBusy {
                    pending,
                    waiting: true,
                };
                let waiting = format!(
                    "Waiting up to {} for the other write transaction{}",
                    self.write_timeout_text(),
                    self.theme.symbol(Symbol::Ellipsis)
                );
                let mut modal = modal.line("").styled(waiting, Role::Warning);
                modal.keys.clear();
                self.modal = Some(modal);
            }
            (ModalKind::WriterBusy { pending, .. }, KeyCode::Esc)
                if matches!(pending.change, Change::Delete { .. }) =>
            {
                self.confirm_delete(pending);
            }
            (ModalKind::Reopen, KeyCode::Char('r')) => self.reopen(),
            (ModalKind::Reopen, KeyCode::Char('q')) => self.quit = true,
            (ModalKind::Corrupt, KeyCode::Char('c')) => self.start_check(),
            (ModalKind::DiscardEdit, KeyCode::Char('y')) => self.close_edit(),
            (ModalKind::Message, KeyCode::Enter | KeyCode::Char('m' | 'q')) => {}
            (ModalKind::Reopen, _) => {
                modal.kind = ModalKind::Reopen;
                self.modal = Some(modal);
            }
            (_, KeyCode::Esc) => {}
            (kind, _) => {
                modal.kind = kind;
                self.modal = Some(modal);
            }
        }
    }

    fn reopen(&mut self) {
        match self.source.reopen() {
            Ok(()) => {
                self.reload();
                let name = self
                    .summary
                    .as_ref()
                    .map(|summary| summary.name.clone())
                    .unwrap_or_default();
                let next = if self.has_unsaved_edit() {
                    "; your edit is still open, confirm it to commit"
                } else {
                    ""
                };
                self.info(format!("Reopened {name}{next}"));
            }
            Err(error) => {
                let line = error_line(&error);
                self.modal = Some(
                    Modal::new(ModalKind::Reopen, "Reopen failed")
                        .styled(line, Role::Error)
                        .key("r", "try again")
                        .key("q", "quit"),
                );
            }
        }
    }

    pub(super) fn close_edit(&mut self) {
        if let Some(focus) = self.edit.take().map(|edit| edit.return_focus()) {
            self.focus = focus;
        }
    }

    fn scroll_modal(&self, modal: &mut Modal, code: KeyCode) -> bool {
        let layout = self.modal_layout(modal);
        let last_top = layout.lines.len().saturating_sub(layout.visible);
        let scroll = &mut modal.scroll;
        match code {
            KeyCode::Down | KeyCode::Char('j') if last_top > 0 => {
                *scroll = (*scroll + 1).min(last_top);
            }
            KeyCode::Up | KeyCode::Char('k') if last_top > 0 => {
                *scroll = scroll.saturating_sub(1);
            }
            KeyCode::PageDown => *scroll = (*scroll + layout.visible).min(last_top),
            KeyCode::PageUp => *scroll = scroll.saturating_sub(layout.visible),
            KeyCode::Home => *scroll = 0,
            KeyCode::End => *scroll = last_top,
            _ => return false,
        }
        true
    }

    fn keys_text(&self, modal: &Modal) -> String {
        modal
            .keys
            .iter()
            .map(|(key, label)| format!("{key} {label}"))
            .collect::<Vec<_>>()
            .join(KEY_GAP)
    }

    fn modal_layout(&self, modal: &Modal) -> Layout {
        let cols = usize::from(self.size.cols);
        let rows = usize::from(self.size.rows);
        let most_text = cols.saturating_sub(8).max(1);
        let widest = modal
            .body
            .iter()
            .map(|line| text_width(&line.text))
            .chain([
                text_width(&self.keys_text(modal)),
                text_width(&modal.title) + 2,
                SMALLEST_TEXT_WIDTH,
            ])
            .max()
            .unwrap_or(SMALLEST_TEXT_WIDTH);
        let text_width = widest.min(most_text);
        let lines: Vec<(String, Role)> = modal
            .body
            .iter()
            .flat_map(|line| {
                let wrapped = match line.wrap {
                    Wrap::AtSpaces => wrap(&line.text, text_width),
                    Wrap::AnyCharacter => wrap_characters(&line.text, text_width),
                };
                wrapped.into_iter().map(move |text| (text, line.role))
            })
            .collect();
        let chrome = if modal.keys.is_empty() { 2 } else { 4 };
        let most_visible = rows.saturating_sub(4 + chrome).max(1);
        let visible = lines.len().clamp(1, most_visible);
        let width = text_width + 4;
        let height = visible + chrome;
        let col = cols.saturating_sub(width) / 2;
        let row = rows.saturating_sub(height) / 2;
        Layout {
            col: u16::try_from(col).unwrap_or(0),
            row: u16::try_from(row).unwrap_or(0),
            width: u16::try_from(width).unwrap_or(self.size.cols),
            text_width,
            lines,
            visible,
        }
    }

    pub(super) fn draw_modal(&self, frame: &mut Frame, modal: &Modal) {
        let layout = self.modal_layout(modal);
        let plain = self.theme.style(Role::Plain);
        let horizontal = self.theme.symbol(Symbol::HorizontalLine);
        let vertical = self.theme.symbol(Symbol::VerticalLine);
        let inner = usize::from(layout.width.saturating_sub(2));
        let top = format!(
            "{} {} {}{}",
            self.theme.symbol(Symbol::TopLeft),
            modal.title,
            horizontal.repeat(inner.saturating_sub(text_width(&modal.title) + 2)),
            self.theme.symbol(Symbol::TopRight)
        );
        let (col, mut row) = (layout.col, layout.row);
        frame.fill(col, row, layout.width, plain);
        self.put(frame, col, row, &top, Role::Plain, layout.width);
        let text_room = u16::try_from(layout.text_width).unwrap_or(layout.width);
        let body_row = |frame: &mut Frame, row: u16, text: &str, role: Role, centred: bool| {
            frame.fill(col, row, layout.width, plain);
            self.put(frame, col, row, vertical, Role::Plain, 1);
            let width = u16::try_from(text_width(text)).unwrap_or(text_room);
            let start = if centred {
                col + 2 + text_room.saturating_sub(width) / 2
            } else {
                col + 2
            };
            self.put(frame, start, row, text, role, text_room);
            self.put(frame, col + layout.width - 1, row, vertical, Role::Plain, 1);
        };
        for (text, role) in layout.lines.iter().skip(modal.scroll).take(layout.visible) {
            row += 1;
            body_row(frame, row, text, *role, false);
        }
        for _ in layout.lines.len()..layout.visible {
            row += 1;
            body_row(frame, row, "", Role::Plain, false);
        }
        if !modal.keys.is_empty() {
            row += 1;
            body_row(frame, row, "", Role::Plain, false);
            row += 1;
            body_row(frame, row, &self.keys_text(modal), Role::Plain, true);
        }
        row += 1;
        let position = if layout.lines.len() > layout.visible {
            format!(
                " {}{}{}/{} ",
                modal.scroll + 1,
                self.theme.symbol(Symbol::EnDash),
                (modal.scroll + layout.visible).min(layout.lines.len()),
                layout.lines.len()
            )
        } else {
            String::new()
        };
        let bottom = format!(
            "{}{}{position}{}{}",
            self.theme.symbol(Symbol::BottomLeft),
            horizontal.repeat(2),
            horizontal.repeat(inner.saturating_sub(text_width(&position) + 2)),
            self.theme.symbol(Symbol::BottomRight)
        );
        frame.fill(col, row, layout.width, plain);
        self.put(frame, col, row, &bottom, Role::Plain, layout.width);
    }
}

#[cfg(test)]
mod tests {
    use super::{wrap, wrap_characters};

    #[test]
    fn wrapping_breaks_at_spaces_and_inside_long_words() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
        assert_eq!(wrap("", 10), [""]);
        assert_eq!(wrap_characters("ab cd", 3), ["ab ", "cd"]);
        assert_eq!(wrap_characters("", 3), [""]);
    }
}
