/// Where copied text goes. The platform layer gives the system clipboard; until it does, and
/// in tests, [`MemoryClipboard`] keeps the text in memory.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_clipboard_keeps_the_last_text() {
        let mut clipboard = MemoryClipboard::default();
        assert_eq!(clipboard.text(), None);
        clipboard.put_text("1\tAda\n");
        clipboard.put_text("2\tGrace\n");
        assert_eq!(clipboard.text().as_deref(), Some("2\tGrace\n"));
    }
}
