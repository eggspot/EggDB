use std::cell::RefCell;
use std::rc::Rc;

use eggdb_platform::window::Window;

/// Where copied text goes: the system clipboard of the platform window
/// ([`SystemClipboard`]), or, in tests and where the platform has none, [`MemoryClipboard`].
pub trait Clipboard {
    /// Replaces the clipboard's contents with `text`.
    fn put_text(&mut self, text: &str);
    /// The clipboard's text, when it holds text.
    fn text(&self) -> Option<String>;
}

/// A clipboard in memory: what the client copies when the platform gives none, and what tests
/// read back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryClipboard {
    held: Option<String>,
}

impl Clipboard for MemoryClipboard {
    fn put_text(&mut self, text: &str) {
        self.held = Some(text.to_owned());
    }

    fn text(&self) -> Option<String> {
        self.held.clone()
    }
}

/// The system clipboard of a platform window, shared with the event loop that owns the
/// window (`NSPasteboard`, the Win32 clipboard, X11's `CLIPBOARD` selection). Copies also stay
/// in memory, so when the platform refuses (another program holds the clipboard, or the
/// system has none) a paste inside the client still gets the last copy.
pub struct SystemClipboard {
    window: Rc<RefCell<Box<dyn Window>>>,
    fallback: MemoryClipboard,
}

impl SystemClipboard {
    /// The clipboard of `window`.
    pub fn new(window: Rc<RefCell<Box<dyn Window>>>) -> Self {
        Self {
            window,
            fallback: MemoryClipboard::default(),
        }
    }
}

impl Clipboard for SystemClipboard {
    fn put_text(&mut self, text: &str) {
        self.fallback.put_text(text);
        if let Ok(mut window) = self.window.try_borrow_mut()
            && let Err(error) = window.put_clipboard_text(text)
        {
            eprintln!("eggdb-gui: the system clipboard refused the copy: {error}");
        }
    }

    fn text(&self) -> Option<String> {
        let answer = self
            .window
            .try_borrow_mut()
            .ok()
            .map(|mut window| window.clipboard_text());
        match answer {
            Some(Ok(text)) => text,
            Some(Err(_)) | None => self.fallback.text(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use eggdb_platform::window::{WindowError, WindowEvent};

    #[derive(Debug, Default)]
    struct FakeWindow {
        clipboard: Option<String>,
        refuses: bool,
    }

    impl Window for FakeWindow {
        fn size(&self) -> (u32, u32) {
            (800, 560)
        }

        fn scale(&self) -> f32 {
            1.0
        }

        fn present(&mut self, _width: u32, _height: u32, _rgba: &[u8]) -> Result<(), WindowError> {
            Ok(())
        }

        fn next_event(
            &mut self,
            _timeout: Option<Duration>,
        ) -> Result<Option<WindowEvent>, WindowError> {
            Ok(None)
        }

        fn put_clipboard_text(&mut self, text: &str) -> Result<(), WindowError> {
            if self.refuses {
                return Err(WindowError::Unsupported("no clipboard"));
            }
            self.clipboard = Some(text.to_owned());
            Ok(())
        }

        fn clipboard_text(&mut self) -> Result<Option<String>, WindowError> {
            if self.refuses {
                return Err(WindowError::Unsupported("no clipboard"));
            }
            Ok(self.clipboard.clone())
        }
    }

    fn shared(window: FakeWindow) -> Rc<RefCell<Box<dyn Window>>> {
        Rc::new(RefCell::new(Box::new(window)))
    }

    #[test]
    fn the_system_clipboard_round_trips_through_the_window() {
        let window = shared(FakeWindow::default());
        let mut clipboard = SystemClipboard::new(Rc::clone(&window));
        assert_eq!(clipboard.text(), None);
        clipboard.put_text("1\tAda\n");
        assert_eq!(clipboard.text().as_deref(), Some("1\tAda\n"));
        let _ = window.borrow_mut().put_clipboard_text("from another program");
        assert_eq!(clipboard.text().as_deref(), Some("from another program"));
    }

    #[test]
    fn a_refusing_platform_keeps_the_copy_in_memory() {
        let mut clipboard = SystemClipboard::new(shared(FakeWindow {
            refuses: true,
            ..FakeWindow::default()
        }));
        clipboard.put_text("SELECT 1;");
        assert_eq!(clipboard.text().as_deref(), Some("SELECT 1;"));
    }

    #[test]
    fn the_memory_clipboard_keeps_the_last_text() {
        let mut clipboard = MemoryClipboard::default();
        assert_eq!(clipboard.text(), None);
        clipboard.put_text("1\tAda\n");
        clipboard.put_text("2\tGrace\n");
        assert_eq!(clipboard.text().as_deref(), Some("2\tGrace\n"));
    }
}
