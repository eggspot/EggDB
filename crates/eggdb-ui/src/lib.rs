//! EggDB's terminal user interface, `eggdb ui`: one database file full screen in any terminal,
//! with a key list and a value viewer (text, JSON and hex). The design is `docs/ux/ui.md`.
//!
//! The UI never talks to the operating system itself. It reads the database through a
//! [`Source`] and draws on a [`Terminal`]: [`OsTerminal`] for the real one (raw mode and
//! restore come from the platform layer), [`FakeTerminal`] for tests, which compare its
//! [`FakeTerminal::screen`] with golden text files.
//!
//! Character widths come from our own range tables, an approximation of Unicode's East Asian
//! Width (Wide and Fullwidth take two cells: CJK, Hangul, fullwidth forms, emoji) and of the
//! nonspacing, enclosing and format characters (zero cells, joined to the cell before);
//! anything else takes one cell.

mod app;
mod decoder;
mod fake;
mod frame;
mod json;
mod os_terminal;
mod render;
mod source;
mod terminal;
mod text;
mod text_view;
mod theme;
mod width;

pub use app::{Ui, UiOptions};
pub use decoder::InputDecoder;
pub use fake::FakeTerminal;
pub use frame::{Cell, CellText, Color, Frame, Style};
pub use os_terminal::{Fallbacks, OsTerminal, detect_capabilities};
pub use render::Renderer;
pub use source::{CountStep, KeyRow, Source, Start, Summary};
pub use terminal::{Capabilities, Colors, Event, KeyCode, KeyEvent, Modifiers, Size, Terminal};

use std::io;
use std::time::Duration;

const IDLE_WAIT: Duration = Duration::from_millis(200);

/// Runs the UI on `terminal` until the user quits: wait for an event, handle it, draw, present;
/// background work (the key count) runs in steps whenever no event is waiting.
pub fn run<T: Terminal, S: Source>(terminal: &mut T, ui: &mut Ui<S>) -> io::Result<()> {
    let mut frame = Frame::new(terminal.size());
    let mut changed = true;
    loop {
        if changed {
            ui.draw(&mut frame);
            terminal.present(&frame)?;
        }
        if ui.should_quit() {
            return Ok(());
        }
        let wait = if ui.is_idle() {
            IDLE_WAIT
        } else {
            Duration::ZERO
        };
        changed = match terminal.next_event(wait)? {
            Some(event) => {
                ui.handle(event);
                while !ui.should_quit()
                    && let Some(event) = terminal.next_event(Duration::ZERO)?
                {
                    ui.handle(event);
                }
                true
            }
            None if ui.is_idle() => false,
            None => {
                ui.tick();
                true
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{Capabilities, Event, FakeTerminal, KeyCode, KeyEvent, Ui, UiOptions, run};
    use crate::frame::Frame;
    use crate::source::{CountStep, KeyRow, Source, Start, Summary};
    use crate::terminal::{Size, Terminal};
    use std::io;
    use std::time::Duration;

    struct Empty;

    impl Source for Empty {
        fn summary(&self) -> Result<Summary, String> {
            Ok(Summary::default())
        }
        fn keys(&self, _: &[u8], _: Start<'_>, _: usize) -> Result<Vec<KeyRow>, String> {
            Ok(Vec::new())
        }
        fn value(&self, _: &[u8]) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
        fn count(&self, _: &[u8], _: Option<&[u8]>, _: u64) -> Result<CountStep, String> {
            Ok(CountStep {
                counted: 0,
                resume_after: None,
            })
        }
    }

    struct IdleThenQuit {
        inner: FakeTerminal,
        idle_polls: usize,
    }

    impl Terminal for IdleThenQuit {
        fn size(&self) -> Size {
            self.inner.size()
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
        fn next_event(&mut self, _: Duration) -> io::Result<Option<Event>> {
            if self.idle_polls == 0 {
                return Ok(Some(Event::Key(KeyEvent::plain(KeyCode::Char('q')))));
            }
            self.idle_polls -= 1;
            Ok(None)
        }
        fn present(&mut self, frame: &Frame) -> io::Result<()> {
            self.inner.present(frame)
        }
    }

    #[test]
    fn idle_timeouts_present_nothing() {
        let mut terminal = IdleThenQuit {
            inner: FakeTerminal::new(80, 24, Capabilities::FULL),
            idle_polls: 50,
        };
        let mut ui = Ui::new(
            Empty,
            UiOptions {
                capabilities: Capabilities::FULL,
            },
            terminal.size(),
        );
        ui.run_until_idle();
        if let Err(error) = run(&mut terminal, &mut ui) {
            panic!("run: {error}");
        }
        assert_eq!(terminal.inner.frames_presented(), 2);
    }
}
