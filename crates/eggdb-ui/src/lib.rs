//! EggDB's terminal user interface, `eggdb ui`: one database file full screen in any terminal,
//! with a key list, a value viewer (text, JSON and hex), editors that change keys in one
//! confirmed commit each, and a check screen. The design is `docs/ux/ui.md`.
//!
//! The UI never talks to the operating system itself. It reads and writes the database
//! through a [`Source`] and draws on a [`Terminal`]: [`OsTerminal`] for the real one (raw mode and
//! restore come from the platform layer), [`FakeTerminal`] for tests, which compare its
//! [`FakeTerminal::screen`] with golden text files.
//!
//! Character widths come from our own range tables, an approximation of Unicode's East Asian
//! Width (Wide and Fullwidth take two cells: CJK, Hangul, fullwidth forms, emoji) and of the
//! nonspacing, enclosing and format characters (zero cells, joined to the cell before);
//! anything else takes one cell.

mod app;
mod decoder;
mod editor;
mod fake;
mod frame;
mod json;
mod line;
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
pub use line::{Completion, InlineTerminal, LineEnd, LineReader, LineTerminal};
pub use os_terminal::{Fallbacks, OsTerminal, detect_capabilities};
pub use render::Renderer;
pub use source::{
    Change, CheckSummary, CountStep, ErrorKind, KeyRow, Source, SourceError, SqlFailure, SqlRun,
    SqlTable, Start, Summary, WriterBusy,
};
pub use terminal::{Capabilities, Colors, Event, KeyCode, KeyEvent, Modifiers, Size, Terminal};

use std::io;
use std::time::Duration;

const IDLE_WAIT: Duration = Duration::from_millis(200);
const QUIET_BEFORE_A_QUESTION: Duration = Duration::from_millis(300);

/// Runs the UI on `terminal` until the user quits: wait for an event, handle it, draw, present.
/// Background steps (the key count, a write waiting for the writer) run whenever no event is
/// waiting; otherwise the loop wakes every 200 ms to follow a running check and expire toasts.
/// After presenting a frame that shows a question, the loop waits for 300 ms without input, and
/// outside an open bracketed paste, before the question takes keys; keys arriving sooner were
/// typed or pasted before anyone could have read it, and are dropped.
pub fn run<T: Terminal, S: Source>(terminal: &mut T, ui: &mut Ui<S>) -> io::Result<()> {
    let mut frame = Frame::new(terminal.size());
    let mut changed = true;
    loop {
        if changed {
            ui.draw(&mut frame);
            terminal.present(&frame)?;
            ui.presented();
        }
        if ui.should_quit() {
            return Ok(());
        }
        let wait = if ui.awaits_quiet() {
            QUIET_BEFORE_A_QUESTION
        } else if ui.has_steps() {
            Duration::ZERO
        } else {
            IDLE_WAIT
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
            None if ui.awaits_quiet() => {
                if !terminal.paste_open() {
                    ui.input_quiet();
                }
                false
            }
            None => ui.tick(),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{Capabilities, Event, FakeTerminal, KeyCode, KeyEvent, Ui, UiOptions, run};
    use crate::frame::Frame;
    use crate::source::{
        Change, CheckSummary, CountStep, ErrorKind, KeyRow, Source, SourceError, Start, Summary,
        WriterBusy,
    };
    use crate::terminal::{Size, Terminal};
    use std::io;
    use std::time::Duration;

    struct Empty;

    impl Source for Empty {
        fn summary(&self) -> Result<Summary, SourceError> {
            Ok(Summary::default())
        }
        fn keys(&self, _: &[u8], _: Start<'_>, _: usize) -> Result<Vec<KeyRow>, SourceError> {
            Ok(Vec::new())
        }
        fn value(&self, _: &[u8]) -> Result<Option<Vec<u8>>, SourceError> {
            Ok(None)
        }
        fn count(&self, _: &[u8], _: Option<&[u8]>, _: u64) -> Result<CountStep, SourceError> {
            Ok(CountStep {
                counted: 0,
                resume_after: None,
            })
        }
        fn write(&mut self, _: &Change, _: WriterBusy) -> Result<String, SourceError> {
            Err(SourceError::new(ErrorKind::ReadOnly, "empty"))
        }
        fn start_check(&mut self) -> Result<(), SourceError> {
            Ok(())
        }
        fn check_outcome(&mut self) -> Option<Result<CheckSummary, SourceError>> {
            Some(Ok(CheckSummary::default()))
        }
        fn reopen(&mut self) -> Result<(), SourceError> {
            Ok(())
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
