use crate::frame::{Cell, CellText, Color, Frame, Style};
use crate::width::is_unprintable;
use std::fmt::Write;

/// Turns frames into the ANSI bytes that bring the screen from the last frame to the next:
/// only the cells that changed, in runs, with a style change only where the style changes.
#[derive(Debug, Default)]
pub struct Renderer {
    previous: Option<Frame>,
    attributes: bool,
}

impl Renderer {
    /// A renderer that has drawn nothing yet. Without `attributes` it never writes a style.
    pub fn new(attributes: bool) -> Self {
        Self {
            previous: None,
            attributes,
        }
    }

    /// Forgets the screen, so the next frame is drawn whole (after a resize).
    pub fn reset(&mut self) {
        self.previous = None;
    }

    /// The bytes that draw `frame`, appended to `out`; returns how many cells were written.
    pub fn render(&mut self, frame: &Frame, out: &mut Vec<u8>) -> usize {
        let mut text = String::new();
        let whole = self
            .previous
            .as_ref()
            .is_none_or(|previous| previous.size != frame.size);
        if whole {
            text.push_str("\x1b[0m\x1b[2J");
        }
        let cols = usize::from(frame.size.cols);
        let mut written = 0;
        let mut style: Option<Style> = None;
        let mut cursor_at: Option<(usize, usize)> = None;
        for row in 0..usize::from(frame.size.rows) {
            for col in 0..cols {
                let index = row * cols + col;
                let cell = frame.cells[index];
                if matches!(cell.text, CellText::WideContinuation) {
                    continue;
                }
                let changed = whole
                    || self.previous.as_ref().is_some_and(|previous| {
                        previous.cells[index] != cell
                            || (matches!(cell.text, CellText::Wide(_))
                                && previous.cells.get(index + 1) != frame.cells.get(index + 1))
                    });
                if !changed {
                    continue;
                }
                if cursor_at != Some((col, row)) {
                    let _ = write!(text, "\x1b[{};{}H", row + 1, col + 1);
                }
                if self.attributes && style != Some(cell.style) {
                    push_style(&mut text, cell.style);
                    style = Some(cell.style);
                }
                push_cell(&mut text, &cell);
                written += 1;
                let width = if matches!(cell.text, CellText::Wide(_)) {
                    2
                } else {
                    1
                };
                cursor_at = Some((col + width, row));
            }
        }
        if self.attributes && style.is_some_and(|style| style != Style::default()) {
            text.push_str("\x1b[0m");
        }
        match frame.cursor {
            Some((col, row)) => {
                let _ = write!(text, "\x1b[{};{}H\x1b[?25h", row + 1, col + 1);
            }
            None => text.push_str("\x1b[?25l"),
        }
        out.extend_from_slice(text.as_bytes());
        self.previous = Some(frame.clone());
        written
    }
}

fn push_cell(text: &mut String, cell: &Cell) {
    match cell.text {
        CellText::Char(c) | CellText::Wide(c) => text.push(printable(c)),
        CellText::Combined(c, mark) => {
            text.push(printable(c));
            if !is_unprintable(mark) {
                text.push(mark);
            }
        }
        CellText::WideContinuation => {}
    }
}

fn printable(character: char) -> char {
    if is_unprintable(character) {
        '?'
    } else {
        character
    }
}

fn push_style(text: &mut String, style: Style) {
    text.push_str("\x1b[0");
    if style.bold {
        text.push_str(";1");
    }
    if style.dim {
        text.push_str(";2");
    }
    if style.underline {
        text.push_str(";4");
    }
    if style.inverse {
        text.push_str(";7");
    }
    if let Color::Ansi(n) = style.fg {
        let n = n % 16;
        let code = if n < 8 {
            30 + u16::from(n)
        } else {
            82 + u16::from(n)
        };
        let _ = write!(text, ";{code}");
    }
    if let Color::Ansi(n) = style.bg {
        let n = n % 16;
        let code = if n < 8 {
            40 + u16::from(n)
        } else {
            92 + u16::from(n)
        };
        let _ = write!(text, ";{code}");
    }
    text.push('m');
}

#[cfg(test)]
mod tests {
    use super::Renderer;
    use crate::frame::{CellText, Color, Frame, Style};
    use crate::terminal::Size;

    fn frame(rows: &[&str]) -> Frame {
        let cols = rows.iter().map(|r| r.chars().count()).max().unwrap_or(0);
        let mut frame = Frame::new(Size {
            cols: cols as u16,
            rows: rows.len() as u16,
        });
        for (row, text) in rows.iter().enumerate() {
            frame.put(0, row as u16, text, Style::default(), cols as u16, '~');
        }
        frame
    }

    #[test]
    fn the_first_frame_is_drawn_whole() {
        let mut renderer = Renderer::new(true);
        let mut out = Vec::new();
        assert_eq!(renderer.render(&frame(&["ab", "cd"]), &mut out), 4);
        let text = String::from_utf8(out).unwrap_or_default();
        assert!(text.starts_with("\x1b[0m\x1b[2J\x1b[1;1H"));
        assert!(text.contains("ab\x1b[2;1Hcd"));
        assert!(text.ends_with("\x1b[?25l"));
    }

    #[test]
    fn only_changed_cells_are_written_again() {
        let mut renderer = Renderer::new(true);
        let mut out = Vec::new();
        renderer.render(&frame(&["abcd", "efgh"]), &mut out);
        out.clear();
        assert_eq!(renderer.render(&frame(&["abXd", "efgh"]), &mut out), 1);
        assert_eq!(
            String::from_utf8(out).unwrap_or_default(),
            "\x1b[1;3H\x1b[0mX\x1b[?25l"
        );
    }

    #[test]
    fn a_new_size_redraws_everything() {
        let mut renderer = Renderer::new(true);
        let mut out = Vec::new();
        renderer.render(&frame(&["ab"]), &mut out);
        assert_eq!(renderer.render(&frame(&["abc"]), &mut out), 3);
    }

    #[test]
    fn styles_become_sgr_sequences_and_are_reset() {
        let mut renderer = Renderer::new(true);
        let mut f = Frame::new(Size { cols: 2, rows: 1 });
        let style = Style {
            bold: true,
            inverse: true,
            fg: Color::Ansi(6),
            bg: Color::Ansi(9),
            ..Style::default()
        };
        f.put(0, 0, "ab", style, 2, '~');
        let mut out = Vec::new();
        renderer.render(&f, &mut out);
        let text = String::from_utf8(out).unwrap_or_default();
        assert!(text.contains("\x1b[0;1;7;36;101mab\x1b[0m"), "{text:?}");
    }

    #[test]
    fn without_attributes_no_style_is_written() {
        let mut renderer = Renderer::new(false);
        let mut f = Frame::new(Size { cols: 2, rows: 1 });
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        f.put(0, 0, "ab", bold, 2, '~');
        let mut out = Vec::new();
        renderer.render(&f, &mut out);
        let text = String::from_utf8(out).unwrap_or_default();
        assert!(!text.contains("\x1b[0;1"));
    }

    #[test]
    fn cells_set_directly_never_write_controls_or_bidi_overrides() {
        let mut renderer = Renderer::new(false);
        let mut f = Frame::new(Size { cols: 3, rows: 1 });
        f.cells[0].text = CellText::Char('\u{202e}');
        f.cells[1].text = CellText::Combined('a', '\u{2067}');
        f.cells[2].text = CellText::Wide('\u{1b}');
        let mut out = Vec::new();
        renderer.render(&f, &mut out);
        let text = String::from_utf8(out).unwrap_or_default();
        assert!(text.contains("?a?"), "{text:?}");
        assert!(!text.contains(['\u{202e}', '\u{2067}']), "{text:?}");
    }

    #[test]
    fn the_cursor_is_placed_and_shown() {
        let mut renderer = Renderer::new(true);
        let mut f = frame(&["ab"]);
        f.cursor = Some((1, 0));
        let mut out = Vec::new();
        renderer.render(&f, &mut out);
        let text = String::from_utf8(out).unwrap_or_default();
        assert!(text.ends_with("\x1b[1;2H\x1b[?25h"));
    }
}
