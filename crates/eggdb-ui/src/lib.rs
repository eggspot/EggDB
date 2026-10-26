//! EggDB's terminal user interface, `eggdb ui`: one database file full screen in any terminal,
//! with a key list and a value viewer (text, JSON and hex). The design is `docs/ux/ui.md`.
//!
//! The UI never talks to the operating system itself. It reads the database through a
//! [`Source`] and draws on a [`Terminal`]: [`OsTerminal`] for the real one (raw mode and
//! restore come from the platform layer), [`FakeTerminal`] for tests, which compare its
//! [`FakeTerminal::screen`] with golden text files.

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
    loop {
        ui.draw(&mut frame);
        terminal.present(&frame)?;
        if ui.should_quit() {
            return Ok(());
        }
        let wait = if ui.is_idle() {
            IDLE_WAIT
        } else {
            Duration::ZERO
        };
        match terminal.next_event(wait)? {
            Some(event) => {
                ui.handle(event);
                while !ui.should_quit()
                    && let Some(event) = terminal.next_event(Duration::ZERO)?
                {
                    ui.handle(event);
                }
            }
            None => ui.tick(),
        }
    }
}
