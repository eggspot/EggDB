use crate::frame::{Frame, Style};
use crate::terminal::{Capabilities, Event, KeyCode, KeyEvent, Modifiers, Size, Terminal};
use std::collections::VecDeque;
use std::io;
use std::time::Duration;

/// A terminal for tests: events are queued by the test, and each presented frame is kept so
/// the screen can be read back as plain text.
#[derive(Debug)]
pub struct FakeTerminal {
    size: Size,
    capabilities: Capabilities,
    events: VecDeque<Event>,
    last: Option<Frame>,
    presented: usize,
    cells_changed: usize,
}

impl FakeTerminal {
    /// A terminal of `cols × rows` that can show `capabilities`.
    pub fn new(cols: u16, rows: u16, capabilities: Capabilities) -> Self {
        Self {
            size: Size { cols, rows },
            capabilities,
            events: VecDeque::new(),
            last: None,
            presented: 0,
            cells_changed: 0,
        }
    }

    /// Queues an event.
    pub fn push(&mut self, event: Event) {
        self.events.push_back(event);
    }

    /// Queues one key event per character.
    pub fn type_str(&mut self, text: &str) {
        for c in text.chars() {
            self.push(Event::Key(KeyEvent::plain(KeyCode::Char(c))));
        }
    }

    /// Queues the key `name`, such as `Down`, `PgDn`, `Shift-Tab`, `Ctrl-C` or `q`. Returns
    /// `false` and queues nothing for a name it does not know.
    pub fn press(&mut self, name: &str) -> bool {
        match parse_key(name) {
            Some(key) => {
                self.push(Event::Key(key));
                true
            }
            None => false,
        }
    }

    /// Changes the size and queues the resize event.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.size = Size { cols, rows };
        self.push(Event::Resize(self.size));
    }

    /// The last presented frame as rows of plain text, each trimmed on the right.
    pub fn screen(&self) -> String {
        let Some(frame) = &self.last else {
            return String::new();
        };
        let mut text = String::new();
        for row in 0..frame.size.rows {
            text.push_str(&frame.row_text(row));
            text.push('\n');
        }
        text
    }

    /// The style of the cell at `row`, `col` in the last presented frame.
    pub fn styled(&self, row: u16, col: u16) -> Style {
        self.last
            .as_ref()
            .and_then(|frame| frame.cell(col, row))
            .map(|cell| cell.style)
            .unwrap_or_default()
    }

    /// How many frames were presented.
    pub fn frames_presented(&self) -> usize {
        self.presented
    }

    /// How many cells the last presented frame changed from the one before it.
    pub fn cells_changed(&self) -> usize {
        self.cells_changed
    }
}

impl Terminal for FakeTerminal {
    fn size(&self) -> Size {
        self.size
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn next_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
        Ok(self.events.pop_front())
    }

    fn present(&mut self, frame: &Frame) -> io::Result<()> {
        self.cells_changed = match &self.last {
            Some(last) if last.size == frame.size => last
                .cells
                .iter()
                .zip(&frame.cells)
                .filter(|(before, after)| before != after)
                .count(),
            _ => frame.cells.len(),
        };
        self.last = Some(frame.clone());
        self.presented += 1;
        Ok(())
    }
}

fn parse_key(name: &str) -> Option<KeyEvent> {
    let mut modifiers = Modifiers::NONE;
    let mut rest = name;
    loop {
        if let Some(after) = rest.strip_prefix("Ctrl-") {
            modifiers.ctrl = true;
            rest = after;
        } else if let Some(after) = rest.strip_prefix("Alt-") {
            modifiers.alt = true;
            rest = after;
        } else if let Some(after) = rest
            .strip_prefix("Shift-")
            .filter(|after| !after.is_empty())
        {
            modifiers.shift = true;
            rest = after;
        } else {
            break;
        }
    }
    let code = match rest {
        "Enter" => KeyCode::Enter,
        "Esc" => KeyCode::Esc,
        "Tab" if modifiers.shift => {
            modifiers.shift = false;
            KeyCode::BackTab
        }
        "Tab" => KeyCode::Tab,
        "Backspace" => KeyCode::Backspace,
        "Delete" => KeyCode::Delete,
        "Insert" => KeyCode::Insert,
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        "Left" => KeyCode::Left,
        "Right" => KeyCode::Right,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PgUp" => KeyCode::PageUp,
        "PgDn" => KeyCode::PageDown,
        "Space" => KeyCode::Char(' '),
        _ => {
            let mut chars = rest.chars();
            match (chars.next(), chars.next()) {
                (Some('F'), Some(_)) => KeyCode::F(rest[1..].parse().ok()?),
                (Some(c), None) => KeyCode::Char(if modifiers.ctrl {
                    c.to_ascii_lowercase()
                } else {
                    c
                }),
                _ => return None,
            }
        }
    };
    Some(KeyEvent { code, modifiers })
}

#[cfg(test)]
mod tests {
    use super::parse_key;
    use crate::terminal::{KeyCode, KeyEvent, Modifiers};

    #[test]
    fn key_names_parse() {
        assert_eq!(parse_key("Down"), Some(KeyEvent::plain(KeyCode::Down)));
        assert_eq!(
            parse_key("Ctrl-C"),
            Some(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: Modifiers::CTRL
            })
        );
        assert_eq!(
            parse_key("Shift-Tab"),
            Some(KeyEvent::plain(KeyCode::BackTab))
        );
        assert_eq!(parse_key("F12"), Some(KeyEvent::plain(KeyCode::F(12))));
        assert_eq!(parse_key("q"), Some(KeyEvent::plain(KeyCode::Char('q'))));
        assert_eq!(parse_key("Nope"), None);
    }
}
