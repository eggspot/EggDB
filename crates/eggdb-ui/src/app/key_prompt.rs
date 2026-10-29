use super::modal::Pending;
use super::{Hint, Pane, READ_ONLY_TOAST, ToastKind, Ui};
use crate::editor::{Editor, Lines, Outcome};
use crate::frame::Frame;
use crate::source::{Change, Source, WriterBusy};
use crate::terminal::KeyEvent;
use crate::text;
use crate::theme::Role;
use crate::time_to_live::{TimeToLiveError, parse_time_to_live};
use crate::width::text_width;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PromptKind {
    TimeToLive { left: Option<Duration> },
    Counter { decrement: bool },
}

#[derive(Debug)]
pub(super) struct KeyPrompt {
    kind: PromptKind,
    key: Vec<u8>,
    editor: Editor,
    return_focus: Pane,
}

impl KeyPrompt {
    fn title(&self) -> String {
        let key = text::escaped(&self.key);
        match self.kind {
            PromptKind::TimeToLive { .. } => format!(" Expire {key} in: "),
            PromptKind::Counter { decrement: false } => format!(" Increment {key} by: "),
            PromptKind::Counter { decrement: true } => format!(" Decrement {key} by: "),
        }
    }
}

fn whole_number(typed: &str) -> Option<i64> {
    if typed.is_empty() {
        return Some(1);
    }
    if !typed.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    typed.parse::<i64>().ok()
}

impl<S: Source> Ui<S> {
    pub(super) fn start_key_prompt(&mut self, kind: PromptKind) {
        if self.read_only() {
            self.info(READ_ONLY_TOAST);
            return;
        }
        let Some(row) = self.rows.get(self.selected) else {
            self.info("No key selected");
            return;
        };
        let kind = match kind {
            PromptKind::TimeToLive { .. } => PromptKind::TimeToLive {
                left: row.time_to_live,
            },
            counter => counter,
        };
        self.prompt = Some(KeyPrompt {
            kind,
            key: row.key.clone(),
            editor: Editor::new(String::new(), Lines::Single),
            return_focus: self.focus,
        });
        self.focus = Pane::Keys;
    }

    pub(super) fn close_prompt(&mut self) {
        if let Some(prompt) = self.prompt.take() {
            self.focus = prompt.return_focus;
        }
    }

    pub(super) fn prompt_paste(&mut self, pasted: &str) {
        if let Some(prompt) = self.prompt.as_mut() {
            prompt.editor.paste_on_one_line(pasted);
        }
    }

    pub(super) fn prompt_key(&mut self, key: KeyEvent) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        if key.modifiers.alt {
            return;
        }
        match prompt.editor.key(key, 1) {
            Outcome::Confirm => self.confirm_prompt(),
            Outcome::Cancel => self.close_prompt(),
            _ => {}
        }
    }

    fn confirm_prompt(&mut self) {
        let Some(prompt) = self.prompt.as_ref() else {
            return;
        };
        let typed = prompt.editor.text().trim().to_owned();
        let key = prompt.key.clone();
        match prompt.kind {
            PromptKind::TimeToLive { left } => self.confirm_time_to_live(key, left, &typed),
            PromptKind::Counter { decrement } => match whole_number(&typed) {
                Some(amount) => {
                    let by = if decrement { -amount } else { amount };
                    let pending = Pending {
                        change: Change::Increment { key, by },
                        replaced_bytes: None,
                    };
                    self.commit(pending, WriterBusy::Fail);
                }
                None => self.toast(
                    ToastKind::Error,
                    format!("Not a whole number: '{}'", text::without_controls(&typed)),
                ),
            },
        }
    }

    fn confirm_time_to_live(&mut self, key: Vec<u8>, left: Option<Duration>, typed: &str) {
        if typed.is_empty() {
            match left {
                Some(left) => self.confirm_expiry(
                    Pending {
                        change: Change::Persist { key },
                        replaced_bytes: None,
                    },
                    Some(left),
                ),
                None => {
                    self.close_prompt();
                    self.info(format!("{} has no time to live", text::escaped(&key)));
                }
            }
            return;
        }
        match parse_time_to_live(typed) {
            Ok(time_to_live) => self.confirm_expiry(
                Pending {
                    change: Change::Expire { key, time_to_live },
                    replaced_bytes: None,
                },
                left,
            ),
            Err(TimeToLiveError::Zero) => self.toast(
                ToastKind::Error,
                "0 is not a time to live; d deletes the key".to_owned(),
            ),
            Err(TimeToLiveError::NotATime) => self.toast(
                ToastKind::Error,
                format!(
                    "Not a time: '{}'; use 30s, 5m, 1h30m, 7d or 1500ms",
                    text::without_controls(typed)
                ),
            ),
        }
    }

    pub(super) fn prompt_hints(&self) -> Option<Vec<Hint>> {
        Some(match self.prompt.as_ref()?.kind {
            PromptKind::TimeToLive { .. } => vec![
                Hint::new("Enter", "continue"),
                Hint::new("Esc", "cancel"),
                Hint::new("30s 5m 1h30m 7d", "times"),
                Hint::new("empty:", "never"),
            ],
            PromptKind::Counter { .. } => vec![
                Hint::new("Enter", "commit"),
                Hint::new("Esc", "cancel"),
                Hint::new("empty:", "1"),
            ],
        })
    }

    pub(super) fn follow_prompt_cursor(&mut self) {
        let cols = if self.two_panes() {
            self.list_cols()
        } else {
            self.size.cols
        };
        if let Some(prompt) = self.prompt.as_mut() {
            let room = usize::from(cols).saturating_sub(text_width(&prompt.title()) + 1);
            prompt.editor.scroll_into_view(1, room);
        }
    }

    pub(super) fn draw_key_prompt(&self, frame: &mut Frame, col: u16, width: u16) -> bool {
        let Some(prompt) = &self.prompt else {
            return false;
        };
        frame.fill(col, 1, width, self.theme.style(Role::FocusedTitle));
        let used = self.put(frame, col, 1, &prompt.title(), Role::FocusedTitle, width);
        let start = col + used;
        let room = width.saturating_sub(used);
        let segments = [(prompt.editor.text().to_owned(), Role::FocusedTitle)];
        self.draw_segments(frame, start, 1, room, &segments, prompt.editor.pan());
        let cursor = prompt
            .editor
            .cursor_cells()
            .saturating_sub(prompt.editor.pan());
        let at = start.saturating_add(u16::try_from(cursor).unwrap_or(u16::MAX));
        frame.cursor = Some((at.min(col + width.saturating_sub(1)), 1));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::whole_number;

    #[test]
    fn an_empty_amount_is_one_and_only_digits_are_amounts() {
        assert_eq!(whole_number(""), Some(1));
        assert_eq!(whole_number("5"), Some(5));
        assert_eq!(whole_number("9223372036854775807"), Some(i64::MAX));
        for bad in ["-5", "+5", "abc", "1.5", "9223372036854775808"] {
            assert_eq!(whole_number(bad), None, "{bad}");
        }
    }
}
