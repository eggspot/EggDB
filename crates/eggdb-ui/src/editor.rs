use crate::terminal::{KeyCode, KeyEvent};
use crate::text;
use crate::width::{is_unprintable, text_width};

pub(crate) const LONGEST_TEXT: usize = 4 << 20;
const LONGEST_HISTORY: usize = 16 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lines {
    Single,
    Multi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Changed,
    Moved,
    Confirm,
    Cancel,
    Help,
    TooLong,
    Ignored,
}

#[derive(Debug, Clone)]
struct Change {
    at: usize,
    removed: String,
    inserted: String,
    cursor_before: usize,
    typing: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Editor {
    text: String,
    cursor: usize,
    lines: Lines,
    original: String,
    undo: Vec<Change>,
    redo: Vec<Change>,
    history_bytes: usize,
    top: usize,
    pan: usize,
    goal_col: Option<usize>,
}

impl Editor {
    pub(crate) fn new(text: String, lines: Lines) -> Self {
        Self {
            cursor: text.len(),
            original: text.clone(),
            text,
            lines,
            undo: Vec::new(),
            redo: Vec::new(),
            history_bytes: 0,
            top: 0,
            pan: 0,
            goal_col: None,
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn replace(&mut self, from: usize, to: usize, inserted: &str) -> Outcome {
        let to = to.min(self.text.len());
        if !self.text.is_char_boundary(from) || !self.text.is_char_boundary(to) || from > to {
            return Outcome::Ignored;
        }
        self.cursor = to;
        let removed = self.remove(from, to);
        let added = self.insert(inserted, false);
        if added == Outcome::Changed {
            Outcome::Changed
        } else {
            removed
        }
    }

    pub(crate) fn delete_word_before(&mut self) -> Outcome {
        self.remove(self.word_start_before(self.cursor), self.cursor)
    }

    pub(crate) fn lines(&self) -> Lines {
        self.lines
    }

    pub(crate) fn is_changed(&self) -> bool {
        self.text != self.original
    }

    pub(crate) fn top(&self) -> usize {
        self.top
    }

    pub(crate) fn pan(&self) -> usize {
        self.pan
    }

    pub(crate) fn line_count(&self) -> usize {
        self.text.split('\n').count()
    }

    pub(crate) fn visible_lines(&self, rows: usize) -> impl Iterator<Item = (usize, &str)> {
        self.text.split('\n').enumerate().skip(self.top).take(rows)
    }

    pub(crate) fn cursor_line(&self) -> usize {
        self.text[..self.cursor].matches('\n').count()
    }

    pub(crate) fn cursor_col(&self) -> usize {
        self.text[self.line_start(self.cursor)..self.cursor]
            .chars()
            .count()
    }

    pub(crate) fn cursor_cells(&self) -> usize {
        text_width(&self.text[self.line_start(self.cursor)..self.cursor])
    }

    pub(crate) fn move_to(&mut self, offset: usize) {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        self.cursor = offset;
        self.goal_col = None;
    }

    pub(crate) fn scroll_into_view(&mut self, rows: usize, cols: usize) {
        let line = self.cursor_line();
        if line < self.top {
            self.top = line;
        } else if rows > 0 && line >= self.top + rows {
            self.top = line + 1 - rows;
        }
        let cells = self.cursor_cells();
        if cells < self.pan {
            self.pan = cells;
        } else if cols > 0 && cells >= self.pan + cols {
            self.pan = cells + 1 - cols;
        }
    }

    pub(crate) fn paste(&mut self, pasted: &str) -> Outcome {
        let spelled = text::literal(&line_feeds(pasted));
        if spelled.contains('\n') {
            self.lines = Lines::Multi;
        }
        self.insert(&spelled, false)
    }

    pub(crate) fn paste_as_text(&mut self, pasted: &str) -> Outcome {
        let kept = text::without_controls(&line_feeds(pasted));
        if kept.contains('\n') {
            self.lines = Lines::Multi;
        }
        self.insert(&kept, false)
    }

    pub(crate) fn paste_on_one_line(&mut self, pasted: &str) -> Outcome {
        self.insert(&text::escaped(line_feeds(pasted).as_bytes()), false)
    }

    pub(crate) fn key(&mut self, key: KeyEvent, page_rows: usize) -> Outcome {
        let ctrl = key.modifiers.ctrl;
        let alt = key.modifiers.alt;
        let vertical = matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        );
        if !vertical {
            self.goal_col = None;
        }
        match key.code {
            KeyCode::Char('s') if ctrl => Outcome::Confirm,
            KeyCode::Char('z') if ctrl => self.undo(),
            KeyCode::Char('y') if ctrl => self.redo(),
            KeyCode::Char('a') if ctrl => self.moved(self.line_start(self.cursor)),
            KeyCode::Char('e') if ctrl => self.moved(self.line_end(self.cursor)),
            KeyCode::Char('u') if ctrl => self.remove(self.line_start(self.cursor), self.cursor),
            KeyCode::Char('k') if ctrl => self.remove(self.cursor, self.line_end(self.cursor)),
            KeyCode::Char(_) if ctrl || alt => Outcome::Ignored,
            KeyCode::Char(c) if is_unprintable(c) => {
                let mut buffer = [0u8; 4];
                let spelled = text::escaped(c.encode_utf8(&mut buffer).as_bytes());
                self.insert(&spelled, false)
            }
            KeyCode::Char(c) => {
                let mut buffer = [0u8; 4];
                self.insert(c.encode_utf8(&mut buffer), !c.is_whitespace())
            }
            KeyCode::Enter if alt => {
                self.lines = Lines::Multi;
                self.insert("\n", false)
            }
            KeyCode::Enter if self.lines == Lines::Single => Outcome::Confirm,
            KeyCode::Enter => self.insert("\n", false),
            KeyCode::Esc => Outcome::Cancel,
            KeyCode::F(1) => Outcome::Help,
            KeyCode::Tab => self.insert("\\t", false),
            KeyCode::Backspace => match self.text[..self.cursor].chars().next_back() {
                Some(c) => self.remove(self.cursor - c.len_utf8(), self.cursor),
                None => Outcome::Ignored,
            },
            KeyCode::Delete => match self.text[self.cursor..].chars().next() {
                Some(c) => self.remove(self.cursor, self.cursor + c.len_utf8()),
                None => Outcome::Ignored,
            },
            KeyCode::Left if ctrl => self.moved(self.word_start_before(self.cursor)),
            KeyCode::Right if ctrl => self.moved(self.word_end_after(self.cursor)),
            KeyCode::Left => match self.text[..self.cursor].chars().next_back() {
                Some(c) => self.moved(self.cursor - c.len_utf8()),
                None => Outcome::Ignored,
            },
            KeyCode::Right => match self.text[self.cursor..].chars().next() {
                Some(c) => self.moved(self.cursor + c.len_utf8()),
                None => Outcome::Ignored,
            },
            KeyCode::Home if ctrl => self.moved(0),
            KeyCode::End if ctrl => self.moved(self.text.len()),
            KeyCode::Home => self.moved(self.line_start(self.cursor)),
            KeyCode::End => self.moved(self.line_end(self.cursor)),
            KeyCode::Up => self.vertical(-1),
            KeyCode::Down => self.vertical(1),
            KeyCode::PageUp => self.vertical(-(page_rows.max(1) as isize)),
            KeyCode::PageDown => self.vertical(page_rows.max(1) as isize),
            _ => Outcome::Ignored,
        }
    }

    fn moved(&mut self, offset: usize) -> Outcome {
        self.cursor = offset;
        Outcome::Moved
    }

    fn vertical(&mut self, by: isize) -> Outcome {
        let col = *self.goal_col.get_or_insert(self.cursor_col());
        let line = self.cursor_line();
        let last = self.line_count() - 1;
        let target = line.saturating_add_signed(by).min(last);
        let start = self
            .text
            .split('\n')
            .take(target)
            .map(|line| line.len() + 1)
            .sum::<usize>();
        let end = self.line_end(start);
        let offset = self.text[start..end]
            .char_indices()
            .nth(col)
            .map_or(end, |(index, _)| start + index);
        self.cursor = offset;
        Outcome::Moved
    }

    fn line_start(&self, offset: usize) -> usize {
        self.text[..offset].rfind('\n').map_or(0, |at| at + 1)
    }

    fn line_end(&self, offset: usize) -> usize {
        self.text[offset..]
            .find('\n')
            .map_or(self.text.len(), |at| offset + at)
    }

    fn word_start_before(&self, offset: usize) -> usize {
        let before = &self.text[..offset];
        let trimmed = before.trim_end();
        trimmed.rfind(char::is_whitespace).map_or(0, |at| {
            at + trimmed[at..].chars().next().map_or(1, char::len_utf8)
        })
    }

    fn word_end_after(&self, offset: usize) -> usize {
        let after = &self.text[offset..];
        let skipped = after.len() - after.trim_start().len();
        let word = &after[skipped..];
        offset + skipped + word.find(char::is_whitespace).unwrap_or(word.len())
    }

    fn insert(&mut self, inserted: &str, typing: bool) -> Outcome {
        if self.text.len() + inserted.len() > LONGEST_TEXT {
            return Outcome::TooLong;
        }
        let at = self.cursor;
        self.text.insert_str(at, inserted);
        self.cursor = at + inserted.len();
        self.clear_redo();
        if let Some(last) = self.undo.last_mut()
            && typing
            && last.typing
            && last.removed.is_empty()
            && last.at + last.inserted.len() == at
        {
            last.inserted.push_str(inserted);
            self.history_bytes += inserted.len();
            self.trim_history();
            return Outcome::Changed;
        }
        self.remember(Change {
            at,
            removed: String::new(),
            inserted: inserted.to_owned(),
            cursor_before: at,
            typing,
        });
        Outcome::Changed
    }

    fn remember(&mut self, change: Change) {
        self.history_bytes += change.removed.len() + change.inserted.len();
        self.undo.push(change);
        self.trim_history();
    }

    fn trim_history(&mut self) {
        let mut dropped = 0;
        let mut oldest_kept = 0;
        for change in &self.undo {
            if self.history_bytes - dropped <= LONGEST_HISTORY {
                break;
            }
            dropped += change.removed.len() + change.inserted.len();
            oldest_kept += 1;
        }
        self.undo.drain(..oldest_kept);
        self.history_bytes -= dropped;
    }

    fn clear_redo(&mut self) {
        for change in self.redo.drain(..) {
            self.history_bytes -= change.removed.len() + change.inserted.len();
        }
    }

    fn remove(&mut self, from: usize, to: usize) -> Outcome {
        if from >= to {
            return Outcome::Ignored;
        }
        let removed = self.text[from..to].to_owned();
        self.text.replace_range(from..to, "");
        self.clear_redo();
        self.remember(Change {
            at: from,
            removed,
            inserted: String::new(),
            cursor_before: self.cursor,
            typing: false,
        });
        self.cursor = from;
        Outcome::Changed
    }

    fn undo(&mut self) -> Outcome {
        let Some(change) = self.undo.pop() else {
            return Outcome::Ignored;
        };
        self.text.replace_range(
            change.at..change.at + change.inserted.len(),
            &change.removed,
        );
        self.cursor = change.cursor_before;
        self.redo.push(change);
        Outcome::Changed
    }

    fn redo(&mut self) -> Outcome {
        let Some(change) = self.redo.pop() else {
            return Outcome::Ignored;
        };
        self.text.replace_range(
            change.at..change.at + change.removed.len(),
            &change.inserted,
        );
        self.cursor = change.at + change.inserted.len();
        self.undo.push(Change {
            typing: false,
            ..change
        });
        Outcome::Changed
    }
}

fn line_feeds(pasted: &str) -> String {
    pasted.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::{Editor, LONGEST_HISTORY, LONGEST_TEXT, Lines, Outcome};
    use crate::terminal::{KeyCode, KeyEvent, Modifiers};

    fn plain(code: KeyCode) -> KeyEvent {
        KeyEvent::plain(code)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: Modifiers::CTRL,
        }
    }

    fn typed(editor: &mut Editor, text: &str) {
        for c in text.chars() {
            editor.key(plain(KeyCode::Char(c)), 10);
        }
    }

    #[test]
    fn typing_inserts_at_the_cursor_and_undo_takes_back_a_word_at_a_time() {
        let mut editor = Editor::new("Ada".to_owned(), Lines::Single);
        typed(&mut editor, " Lovelace");
        assert_eq!(editor.text(), "Ada Lovelace");
        assert!(editor.is_changed());
        assert_eq!(editor.key(ctrl('z'), 10), Outcome::Changed);
        assert_eq!(editor.text(), "Ada ");
        editor.key(ctrl('z'), 10);
        assert_eq!(editor.text(), "Ada");
        assert!(!editor.is_changed());
        assert_eq!(editor.key(ctrl('z'), 10), Outcome::Ignored);
        editor.key(ctrl('y'), 10);
        editor.key(ctrl('y'), 10);
        assert_eq!(editor.text(), "Ada Lovelace");
        assert_eq!(editor.cursor_col(), 12);
    }

    #[test]
    fn enter_confirms_one_line_and_breaks_lines_in_many() {
        let mut editor = Editor::new("a".to_owned(), Lines::Single);
        assert_eq!(editor.key(plain(KeyCode::Enter), 10), Outcome::Confirm);
        let alt_enter = KeyEvent {
            code: KeyCode::Enter,
            modifiers: Modifiers::ALT,
        };
        assert_eq!(editor.key(alt_enter, 10), Outcome::Changed);
        assert_eq!(editor.lines(), Lines::Multi);
        typed(&mut editor, "b");
        editor.key(plain(KeyCode::Enter), 10);
        typed(&mut editor, "c");
        assert_eq!(editor.text(), "a\nb\nc");
        assert_eq!((editor.cursor_line(), editor.cursor_col()), (2, 1));
        assert_eq!(editor.key(ctrl('s'), 10), Outcome::Confirm);
    }

    #[test]
    fn lines_keep_their_column_moving_up_and_down() {
        let mut editor = Editor::new("abcdef\nxy\nlonger line".to_owned(), Lines::Multi);
        editor.key(KeyEvent::plain(KeyCode::Up), 10);
        assert_eq!((editor.cursor_line(), editor.cursor_col()), (1, 2));
        editor.key(KeyEvent::plain(KeyCode::Up), 10);
        assert_eq!((editor.cursor_line(), editor.cursor_col()), (0, 6));
        editor.key(KeyEvent::plain(KeyCode::PageDown), 10);
        assert_eq!((editor.cursor_line(), editor.cursor_col()), (2, 11));
        editor.key(KeyEvent::plain(KeyCode::Home), 10);
        assert_eq!(editor.cursor_col(), 0);
        let ctrl_home = KeyEvent {
            code: KeyCode::Home,
            modifiers: Modifiers::CTRL,
        };
        editor.key(ctrl_home, 10);
        assert_eq!(editor.cursor_line(), 0);
    }

    #[test]
    fn line_and_word_deletes() {
        let mut editor = Editor::new("one two three".to_owned(), Lines::Single);
        let ctrl_left = KeyEvent {
            code: KeyCode::Left,
            modifiers: Modifiers::CTRL,
        };
        editor.key(ctrl_left, 10);
        assert_eq!(editor.cursor_col(), 8);
        editor.key(ctrl('k'), 10);
        assert_eq!(editor.text(), "one two ");
        editor.key(ctrl_left, 10);
        editor.key(ctrl('u'), 10);
        assert_eq!(editor.text(), "two ");
        editor.key(plain(KeyCode::Delete), 10);
        assert_eq!(editor.text(), "wo ");
        editor.key(plain(KeyCode::End), 10);
        editor.key(plain(KeyCode::Backspace), 10);
        assert_eq!(editor.text(), "wo");
        editor.key(ctrl('z'), 10);
        assert_eq!(editor.text(), "wo ");
    }

    #[test]
    fn invisible_characters_typed_or_pasted_are_spelled_as_escapes() {
        let mut editor = Editor::new(String::new(), Lines::Single);
        editor.key(plain(KeyCode::Char('\u{202e}')), 10);
        editor.key(plain(KeyCode::Tab), 10);
        assert_eq!(editor.text(), "\\u{202e}\\t");
        assert_eq!(editor.paste("a\\b"), Outcome::Changed);
        assert_eq!(editor.lines(), Lines::Single);
        editor.paste("x\ny");
        assert_eq!(editor.lines(), Lines::Multi);
        assert_eq!(editor.text(), "\\u{202e}\\ta\\\\bx\ny");
    }

    #[test]
    fn pasted_line_breaks_become_line_feeds_whatever_the_terminal_sent() {
        let mut editor = Editor::new(String::new(), Lines::Single);
        editor.paste("{\r\n  \"a\": \"x\\ny\"\r}\n");
        assert_eq!(editor.lines(), Lines::Multi);
        assert_eq!(editor.text(), "{\n  \"a\": \"x\\\\ny\"\n}\n");
        let mut key = Editor::new(String::new(), Lines::Single);
        key.paste_on_one_line("a\r\nb\rc");
        assert_eq!(key.lines(), Lines::Single);
        assert_eq!(key.text(), "a\\nb\\nc");
    }

    #[test]
    fn the_undo_history_keeps_only_the_newest_16_mib() {
        let mut editor = Editor::new(String::new(), Lines::Single);
        let chunk = "z".repeat(1 << 20);
        for _ in 0..12 {
            editor.paste(&chunk);
            editor.key(ctrl('u'), 10);
        }
        assert!(editor.history_bytes <= LONGEST_HISTORY);
        assert_eq!(editor.history_bytes, (16 << 20));
        let mut undone = 0;
        while editor.key(ctrl('z'), 10) == Outcome::Changed {
            undone += 1;
        }
        assert_eq!(undone, 16);
        assert_eq!(editor.text(), "");
        editor.key(ctrl('y'), 10);
        editor.paste("a");
        assert_eq!(editor.history_bytes, (1 << 20) + 1);
    }

    #[test]
    fn the_text_is_bounded() {
        let mut editor = Editor::new("x".repeat(LONGEST_TEXT), Lines::Single);
        assert_eq!(editor.key(plain(KeyCode::Char('y')), 10), Outcome::TooLong);
        assert_eq!(editor.paste("y"), Outcome::TooLong);
        assert_eq!(editor.text().len(), LONGEST_TEXT);
    }

    #[test]
    fn the_view_follows_the_cursor() {
        let mut editor = Editor::new("0123456789".to_owned(), Lines::Single);
        editor.scroll_into_view(1, 4);
        assert_eq!(editor.pan(), 7);
        editor.key(plain(KeyCode::Home), 10);
        editor.scroll_into_view(1, 4);
        assert_eq!(editor.pan(), 0);
        let mut many = Editor::new("a\nb\nc\nd\ne".to_owned(), Lines::Multi);
        many.scroll_into_view(2, 10);
        assert_eq!(many.top(), 3);
        assert_eq!(
            many.visible_lines(2).collect::<Vec<_>>(),
            [(3, "d"), (4, "e")]
        );
        many.move_to(0);
        many.scroll_into_view(2, 10);
        assert_eq!(many.top(), 0);
    }
}
