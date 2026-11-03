const TAB_SPACES: usize = 4;
const MAX_UNDO: usize = 1000;

/// A key the editor understands, already translated from the platform's key events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A typed character.
    Char(char),
    /// Return.
    Enter,
    /// Backspace.
    Backspace,
    /// Forward delete.
    Delete,
    /// Arrow left; with `select`, extends the selection.
    Left {
        /// Whether Shift is held.
        select: bool,
    },
    /// Arrow right.
    Right {
        /// Whether Shift is held.
        select: bool,
    },
    /// Arrow up.
    Up {
        /// Whether Shift is held.
        select: bool,
    },
    /// Arrow down.
    Down {
        /// Whether Shift is held.
        select: bool,
    },
    /// Home: the start of the line.
    Home,
    /// End: the end of the line.
    End,
    /// Tab: four spaces.
    Tab,
    /// Select everything.
    SelectAll,
    /// Undo the last change.
    Undo,
}

/// A place in the text: a line and a character column, both from 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Position {
    /// The line.
    pub line: usize,
    /// The column, in characters.
    pub column: usize,
}

/// The query editor's text, caret, selection and undo history (`docs/ux/gui.md`, "Text input
/// and the editor"): `Tab` inserts four spaces, brackets and quotes close themselves, and undo
/// is unlimited up to a thousand steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    lines: Vec<String>,
    caret: Position,
    anchor: Option<Position>,
    undo: Vec<(Vec<String>, Position)>,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new("")
    }
}

fn byte_at(line: &str, column: usize) -> usize {
    line.char_indices()
        .nth(column)
        .map_or(line.len(), |(at, _)| at)
}

fn closing(character: char) -> Option<char> {
    match character {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '\'' => Some('\''),
        '"' => Some('"'),
        _ => None,
    }
}

impl Editor {
    /// An editor holding `text`, the caret at its end.
    pub fn new(text: &str) -> Self {
        let lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        let last = lines.len() - 1;
        let caret = Position {
            line: last,
            column: lines[last].chars().count(),
        };
        Self {
            lines,
            caret,
            anchor: None,
            undo: Vec::new(),
        }
    }

    /// The text, lines joined with `\n`.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// The lines.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The caret.
    pub fn caret(&self) -> Position {
        self.caret
    }

    /// The selection, start before end, when there is one.
    pub fn selection(&self) -> Option<(Position, Position)> {
        let anchor = self.anchor?;
        (anchor != self.caret).then(|| (anchor.min(self.caret), anchor.max(self.caret)))
    }

    /// The selected text, or the whole text when nothing is selected: what `Run` runs.
    pub fn runnable_text(&self) -> String {
        match self.selection() {
            Some((start, end)) => self.slice(start, end),
            None => self.text(),
        }
    }

    fn line_length(&self, line: usize) -> usize {
        self.lines[line].chars().count()
    }

    fn slice(&self, start: Position, end: Position) -> String {
        let mut out = String::new();
        for line in start.line..=end.line {
            let text = &self.lines[line];
            let from = if line == start.line {
                byte_at(text, start.column)
            } else {
                0
            };
            let to = if line == end.line {
                byte_at(text, end.column)
            } else {
                text.len()
            };
            out.push_str(&text[from..to]);
            if line != end.line {
                out.push('\n');
            }
        }
        out
    }

    fn remember(&mut self) {
        if self.undo.len() >= MAX_UNDO {
            self.undo.remove(0);
        }
        self.undo.push((self.lines.clone(), self.caret));
    }

    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        let tail = self.lines[end.line][byte_at(&self.lines[end.line], end.column)..].to_owned();
        let head_end = byte_at(&self.lines[start.line], start.column);
        self.lines[start.line].truncate(head_end);
        self.lines[start.line].push_str(&tail);
        self.lines.drain(start.line + 1..=end.line);
        self.caret = start;
        self.anchor = None;
        true
    }

    fn insert(&mut self, text: &str) {
        self.delete_selection();
        let line = &mut self.lines[self.caret.line];
        let at = byte_at(line, self.caret.column);
        let tail = line.split_off(at);
        let mut pieces = text.split('\n');
        if let Some(first) = pieces.next() {
            line.push_str(first);
            self.caret.column += first.chars().count();
        }
        for piece in pieces {
            self.caret.line += 1;
            self.lines.insert(self.caret.line, piece.to_owned());
            self.caret.column = piece.chars().count();
        }
        let line = &mut self.lines[self.caret.line];
        line.push_str(&tail);
    }

    fn moved(&mut self, select: bool, to: Position) {
        if select {
            self.anchor.get_or_insert(self.caret);
        } else {
            self.anchor = None;
        }
        self.caret = to;
    }

    /// The byte offset in [`Editor::text`] of `at`, clamped to the text.
    pub fn offset(&self, at: Position) -> usize {
        let line = at.line.min(self.lines.len() - 1);
        let before: usize = self.lines[..line].iter().map(|text| text.len() + 1).sum();
        before + byte_at(&self.lines[line], at.column)
    }

    /// The position of byte `offset` of [`Editor::text`], clamped to the text.
    pub fn position_at(&self, offset: usize) -> Position {
        let mut left = offset;
        for (line, text) in self.lines.iter().enumerate() {
            if left <= text.len() {
                let mut at = left;
                while !text.is_char_boundary(at) {
                    at -= 1;
                }
                return Position {
                    line,
                    column: text[..at].chars().count(),
                };
            }
            left -= text.len() + 1;
        }
        let line = self.lines.len() - 1;
        Position {
            line,
            column: self.line_length(line),
        }
    }

    /// The selection as a byte range of [`Editor::text`], when there is one.
    pub fn selected_bytes(&self) -> Option<std::ops::Range<usize>> {
        self.selection()
            .map(|(start, end)| self.offset(start)..self.offset(end))
    }

    fn clamped(&self, at: Position) -> Position {
        let line = at.line.min(self.lines.len() - 1);
        Position {
            line,
            column: at.column.min(self.line_length(line)),
        }
    }

    /// Moves the caret to `at` (clamped to the text), selecting from where the selection
    /// started, or from the caret when nothing was selected: a pointer drag.
    pub fn select_to(&mut self, at: Position) {
        let to = self.clamped(at);
        self.moved(true, to);
    }

    /// Selects `start` to `end` of line `line` (columns, clamped), the caret at the end: a
    /// double-click's word.
    pub fn select_columns(&mut self, line: usize, start: usize, end: usize) {
        let from = self.clamped(Position {
            line,
            column: start,
        });
        let to = self.clamped(Position { line, column: end });
        self.anchor = Some(from);
        self.caret = to;
    }

    /// Selects line `line` with its line break, the caret at the next line's start (the end
    /// of the text on the last line): a triple-click.
    pub fn select_line(&mut self, line: usize) {
        let line = line.min(self.lines.len() - 1);
        self.anchor = Some(Position { line, column: 0 });
        self.caret = if line + 1 < self.lines.len() {
            Position {
                line: line + 1,
                column: 0,
            }
        } else {
            Position {
                line,
                column: self.line_length(line),
            }
        };
    }

    /// The selected text, `None` when nothing is selected.
    pub fn selected_text(&self) -> Option<String> {
        self.selection().map(|(start, end)| self.slice(start, end))
    }

    /// Inserts `text` at the caret, replacing the selection, as one undo step; the caret ends
    /// after it. Line breaks are `\n`.
    pub fn paste(&mut self, text: &str) {
        self.remember();
        self.delete_selection();
        let line = self.caret.line;
        let at = byte_at(&self.lines[line], self.caret.column);
        let tail = self.lines[line].split_off(at);
        let mut pieces = text.split('\n');
        let first = pieces.next().unwrap_or_default();
        self.lines[line].push_str(first);
        let rest: Vec<String> = pieces.map(str::to_owned).collect();
        if rest.is_empty() {
            self.caret.column += first.chars().count();
            self.lines[line].push_str(&tail);
            return;
        }
        let last = line + rest.len();
        self.lines.splice(line + 1..line + 1, rest);
        self.caret = Position {
            line: last,
            column: self.line_length(last),
        };
        self.lines[last].push_str(&tail);
    }

    /// Puts the caret at `at` (clamped to the text) and clears the selection.
    pub fn place_caret(&mut self, at: Position) {
        let line = at.line.min(self.lines.len() - 1);
        self.caret = Position {
            line,
            column: at.column.min(self.line_length(line)),
        };
        self.anchor = None;
    }

    /// Applies `key`.
    pub fn apply(&mut self, key: Key) {
        match key {
            Key::Char(character) => {
                self.remember();
                self.insert(&character.to_string());
                if let Some(close) = closing(character) {
                    let line = &mut self.lines[self.caret.line];
                    let at = byte_at(line, self.caret.column);
                    line.insert(at, close);
                }
            }
            Key::Tab => {
                self.remember();
                self.insert(&" ".repeat(TAB_SPACES));
            }
            Key::Enter => {
                self.remember();
                self.insert("\n");
            }
            Key::Backspace => {
                self.remember();
                if self.delete_selection() {
                    return;
                }
                if self.caret.column > 0 {
                    self.anchor = Some(Position {
                        column: self.caret.column - 1,
                        ..self.caret
                    });
                    self.delete_selection();
                } else if self.caret.line > 0 {
                    let previous = self.caret.line - 1;
                    self.anchor = Some(Position {
                        line: previous,
                        column: self.line_length(previous),
                    });
                    self.delete_selection();
                }
            }
            Key::Delete => {
                self.remember();
                if self.delete_selection() {
                    return;
                }
                let end = if self.caret.column < self.line_length(self.caret.line) {
                    Position {
                        column: self.caret.column + 1,
                        ..self.caret
                    }
                } else if self.caret.line + 1 < self.lines.len() {
                    Position {
                        line: self.caret.line + 1,
                        column: 0,
                    }
                } else {
                    return;
                };
                self.anchor = Some(end);
                self.delete_selection();
            }
            Key::Left { select } => {
                let to = if self.caret.column > 0 {
                    Position {
                        column: self.caret.column - 1,
                        ..self.caret
                    }
                } else if self.caret.line > 0 {
                    Position {
                        line: self.caret.line - 1,
                        column: self.line_length(self.caret.line - 1),
                    }
                } else {
                    self.caret
                };
                self.moved(select, to);
            }
            Key::Right { select } => {
                let to = if self.caret.column < self.line_length(self.caret.line) {
                    Position {
                        column: self.caret.column + 1,
                        ..self.caret
                    }
                } else if self.caret.line + 1 < self.lines.len() {
                    Position {
                        line: self.caret.line + 1,
                        column: 0,
                    }
                } else {
                    self.caret
                };
                self.moved(select, to);
            }
            Key::Up { select } => {
                let line = self.caret.line.saturating_sub(1);
                let to = Position {
                    line,
                    column: self.caret.column.min(self.line_length(line)),
                };
                self.moved(select, to);
            }
            Key::Down { select } => {
                let line = (self.caret.line + 1).min(self.lines.len() - 1);
                let to = Position {
                    line,
                    column: self.caret.column.min(self.line_length(line)),
                };
                self.moved(select, to);
            }
            Key::Home => self.moved(
                false,
                Position {
                    column: 0,
                    ..self.caret
                },
            ),
            Key::End => {
                let column = self.line_length(self.caret.line);
                self.moved(
                    false,
                    Position {
                        column,
                        ..self.caret
                    },
                );
            }
            Key::SelectAll => {
                self.anchor = Some(Position::default());
                let line = self.lines.len() - 1;
                self.caret = Position {
                    line,
                    column: self.line_length(line),
                };
            }
            Key::Undo => {
                if let Some((lines, caret)) = self.undo.pop() {
                    self.lines = lines;
                    self.caret = caret;
                    self.anchor = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(editor: &mut Editor, text: &str) {
        for character in text.chars() {
            editor.apply(Key::Char(character));
        }
    }

    #[test]
    fn offsets_and_positions_convert_both_ways() {
        let mut editor = Editor::new("SELECT 1;\nSELECT 'é', 2;");
        assert_eq!(editor.offset(Position { line: 1, column: 0 }), 10);
        assert_eq!(editor.offset(Position { line: 1, column: 9 }), 20);
        assert_eq!(editor.position_at(20), Position { line: 1, column: 9 });
        assert_eq!(editor.position_at(18), Position { line: 1, column: 8 });
        assert_eq!(editor.position_at(9), Position { line: 0, column: 9 });
        assert_eq!(
            editor.position_at(999),
            Position {
                line: 1,
                column: 14
            }
        );
        editor.place_caret(Position {
            line: 7,
            column: 99,
        });
        assert_eq!(
            editor.caret(),
            Position {
                line: 1,
                column: 14
            }
        );
        editor.apply(Key::Left { select: true });
        assert_eq!(editor.selected_bytes(), Some(24..25));
    }

    #[test]
    fn typing_editing_and_undo_change_the_text_as_an_editor_should() {
        let mut editor = Editor::default();
        typed(&mut editor, "SELECT count");
        editor.apply(Key::Char('('));
        assert_eq!(editor.text(), "SELECT count()");
        typed(&mut editor, "*");
        assert_eq!(editor.text(), "SELECT count(*)");
        editor.apply(Key::End);
        editor.apply(Key::Enter);
        editor.apply(Key::Tab);
        typed(&mut editor, "FROM t");
        assert_eq!(editor.text(), "SELECT count(*)\n    FROM t");
        editor.apply(Key::Backspace);
        assert_eq!(editor.text(), "SELECT count(*)\n    FROM ");
        editor.apply(Key::Undo);
        assert_eq!(editor.text(), "SELECT count(*)\n    FROM t");
        editor.apply(Key::Home);
        editor.apply(Key::Backspace);
        assert_eq!(editor.text(), "SELECT count(*)    FROM t");
    }

    #[test]
    fn a_selection_is_what_runs_and_typing_replaces_it() {
        let mut editor = Editor::new("SELECT 1;\nSELECT 2;");
        assert_eq!(editor.runnable_text(), "SELECT 1;\nSELECT 2;");
        for _ in 0..9 {
            editor.apply(Key::Left { select: true });
        }
        assert_eq!(editor.runnable_text(), "SELECT 2;");
        editor.apply(Key::Char('x'));
        assert_eq!(editor.text(), "SELECT 1;\nx");
        editor.apply(Key::SelectAll);
        editor.apply(Key::Delete);
        assert_eq!(editor.text(), "");
        editor.apply(Key::Up { select: false });
        editor.apply(Key::Delete);
        assert_eq!(editor.caret(), Position::default());
    }

    #[test]
    fn multibyte_text_moves_by_characters() {
        let mut editor = Editor::new("é😀");
        editor.apply(Key::Left { select: false });
        editor.apply(Key::Backspace);
        assert_eq!(editor.text(), "😀");
    }

    #[test]
    fn a_paste_replaces_the_selection_as_one_undo_step() {
        let mut editor = Editor::new("SELECT 1 FROM t;");
        editor.select_columns(0, 7, 8);
        editor.paste("id,\n  name");
        assert_eq!(editor.text(), "SELECT id,\n  name FROM t;");
        assert_eq!(editor.caret(), Position { line: 1, column: 6 });
        editor.paste("!");
        assert_eq!(editor.text(), "SELECT id,\n  name! FROM t;");
        editor.apply(Key::Undo);
        editor.apply(Key::Undo);
        assert_eq!(editor.text(), "SELECT 1 FROM t;");
    }

    #[test]
    fn dragging_selects_from_the_press_and_clicks_select_words_and_lines() {
        let mut editor = Editor::new("SELECT 1;\nSELECT 2;");
        editor.place_caret(Position { line: 0, column: 2 });
        editor.select_to(Position { line: 1, column: 3 });
        assert_eq!(editor.selected_text().as_deref(), Some("LECT 1;\nSEL"));
        editor.select_to(Position { line: 0, column: 0 });
        assert_eq!(editor.selected_text().as_deref(), Some("SE"));
        editor.select_to(Position {
            line: 9,
            column: 99,
        });
        assert_eq!(editor.caret(), Position { line: 1, column: 9 });
        editor.select_line(0);
        assert_eq!(editor.selected_text().as_deref(), Some("SELECT 1;\n"));
        editor.select_line(1);
        assert_eq!(editor.selected_text().as_deref(), Some("SELECT 2;"));
        editor.place_caret(Position { line: 0, column: 0 });
        assert_eq!(editor.selected_text(), None);
    }
}
