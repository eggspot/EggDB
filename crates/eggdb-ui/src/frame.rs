use crate::terminal::Size;
use crate::width::char_width;

/// A full screen of cells, drawn by the UI and shown by [`crate::Terminal::present`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The screen's size.
    pub size: Size,
    /// The cells, row by row, `size.cols × size.rows` of them.
    pub cells: Vec<Cell>,
    /// Where the cursor shows, as `(col, row)`; `None` hides it.
    pub cursor: Option<(u16, u16)>,
}

/// One character cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Cell {
    /// What the cell shows.
    pub text: CellText,
    /// How it shows it.
    pub style: Style,
}

/// What a cell shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellText {
    /// One character one cell wide.
    Char(char),
    /// One character two cells wide; the next cell is its continuation.
    Wide(char),
    /// The second cell of a wide character.
    WideContinuation,
    /// A character followed by a combining mark.
    Combined(char, char),
}

impl Default for CellText {
    fn default() -> Self {
        Self::Char(' ')
    }
}

/// How a cell is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Style {
    /// Foreground colour.
    pub fg: Color,
    /// Background colour.
    pub bg: Color,
    /// Bold.
    pub bold: bool,
    /// Foreground and background swapped.
    pub inverse: bool,
    /// Dimmed.
    pub dim: bool,
    /// Underlined.
    pub underline: bool,
}

/// A colour: the terminal's default or one of the 16 ANSI colours (0 to 15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Color {
    /// The terminal's default colour.
    #[default]
    Default,
    /// ANSI colour 0 to 15 (8 to 15 are the bright ones).
    Ansi(u8),
}

impl Frame {
    /// A blank frame of `size`.
    pub fn new(size: Size) -> Self {
        Self {
            size,
            cells: vec![Cell::default(); usize::from(size.cols) * usize::from(size.rows)],
            cursor: None,
        }
    }

    /// The cell at `col`, `row`, if it is on the screen.
    pub fn cell(&self, col: u16, row: u16) -> Option<&Cell> {
        if col >= self.size.cols || row >= self.size.rows {
            return None;
        }
        self.cells
            .get(usize::from(row) * usize::from(self.size.cols) + usize::from(col))
    }

    /// The row as plain text, trimmed on the right, wide characters once.
    pub fn row_text(&self, row: u16) -> String {
        let mut text = String::new();
        for col in 0..self.size.cols {
            match self.cell(col, row).map(|cell| cell.text) {
                Some(CellText::Char(c) | CellText::Wide(c)) => text.push(c),
                Some(CellText::Combined(c, mark)) => {
                    text.push(c);
                    text.push(mark);
                }
                Some(CellText::WideContinuation) | None => {}
            }
        }
        text.truncate(text.trim_end().len());
        text
    }

    pub(crate) fn clear(&mut self) {
        self.cells.fill(Cell::default());
        self.cursor = None;
    }

    fn index(&self, col: u16, row: u16) -> Option<usize> {
        (col < self.size.cols && row < self.size.rows)
            .then(|| usize::from(row) * usize::from(self.size.cols) + usize::from(col))
    }

    pub(crate) fn fill(&mut self, col: u16, row: u16, width: u16, style: Style) {
        for c in col..col.saturating_add(width).min(self.size.cols) {
            if let Some(index) = self.index(c, row) {
                self.cells[index] = Cell {
                    text: CellText::Char(' '),
                    style,
                };
            }
        }
    }

    pub(crate) fn put(
        &mut self,
        col: u16,
        row: u16,
        text: &str,
        style: Style,
        max_cols: u16,
        ellipsis: char,
    ) -> u16 {
        let end = col.saturating_add(max_cols).min(self.size.cols);
        let room = usize::from(end.saturating_sub(col));
        let cut = crate::width::text_width(text) > room;
        let limit = if cut { room.saturating_sub(1) } else { room };
        let mut at = col;
        let mut used = 0usize;
        for character in text.chars() {
            let character = if character.is_control() {
                '?'
            } else {
                character
            };
            let width = char_width(character);
            if width == 0 {
                if at > col
                    && let Some(index) = self.index(at - 1, row)
                    && let CellText::Char(previous) = self.cells[index].text
                {
                    self.cells[index].text = CellText::Combined(previous, character);
                }
                continue;
            }
            if used + width > limit {
                break;
            }
            if width == 2 {
                self.set(at, row, CellText::Wide(character), style);
                self.set(at + 1, row, CellText::WideContinuation, style);
            } else {
                self.set(at, row, CellText::Char(character), style);
            }
            at += if width == 2 { 2 } else { 1 };
            used += width;
        }
        if cut && at < end {
            self.set(at, row, CellText::Char(ellipsis), style);
            at += 1;
        }
        at - col
    }

    fn set(&mut self, col: u16, row: u16, text: CellText, style: Style) {
        if let Some(index) = self.index(col, row) {
            self.cells[index] = Cell { text, style };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CellText, Frame, Style};
    use crate::terminal::Size;

    fn frame(cols: u16) -> Frame {
        Frame::new(Size { cols, rows: 1 })
    }

    #[test]
    fn text_that_fits_is_drawn_whole() {
        let mut f = frame(10);
        assert_eq!(f.put(0, 0, "user:1", Style::default(), 10, '…'), 6);
        assert_eq!(f.row_text(0), "user:1");
    }

    #[test]
    fn text_that_does_not_fit_ends_with_an_ellipsis() {
        let mut f = frame(10);
        assert_eq!(f.put(0, 0, "abcdefghijkl", Style::default(), 6, '…'), 6);
        assert_eq!(f.row_text(0), "abcde…");
    }

    #[test]
    fn text_exactly_as_wide_as_the_room_has_no_ellipsis() {
        let mut f = frame(10);
        f.put(0, 0, "abcdef", Style::default(), 6, '~');
        assert_eq!(f.row_text(0), "abcdef");
    }

    #[test]
    fn a_wide_character_without_room_becomes_the_ellipsis() {
        let mut f = frame(10);
        f.put(0, 0, "ab日本", Style::default(), 5, '…');
        assert_eq!(f.row_text(0), "ab日…");
        assert_eq!(f.cell(2, 0).map(|c| c.text), Some(CellText::Wide('日')));
        assert_eq!(
            f.cell(3, 0).map(|c| c.text),
            Some(CellText::WideContinuation)
        );
    }

    #[test]
    fn control_characters_never_reach_a_cell() {
        let mut f = frame(10);
        f.put(0, 0, "a\u{1b}[2J\u{9b}b", Style::default(), 10, '~');
        assert_eq!(f.row_text(0), "a?[2J?b");
    }

    #[test]
    fn combining_marks_join_the_previous_cell() {
        let mut f = frame(10);
        assert_eq!(f.put(0, 0, "e\u{301}x", Style::default(), 10, '…'), 2);
        assert_eq!(
            f.cell(0, 0).map(|c| c.text),
            Some(CellText::Combined('e', '\u{301}'))
        );
    }
}
